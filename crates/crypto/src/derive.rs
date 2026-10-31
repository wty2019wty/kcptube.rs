// SPDX-License-Identifier: GPL-3.0-only

//! 密钥与 IV 派生（对应 C++ `shares/aead.hpp`）。
//!
//! 规则（务必与 C++ 保持一致）：
//! - key = SHA3-256(password)
//! - `aes-gcm`   : iv(16B) = head_tail_xor(SHA3-256(password))
//! - `aes-ocb`   : iv(12B) = 反复 head_tail_xor(SHA3-384(password))，直到长度 12
//! - `chacha20`  : iv(8B)  = 反复 head_tail_xor(SHA3-256(password))，直到长度 8
//! - `xchacha20` : iv(24B) = head_tail_xor(SHA3-384(password))
//!
//! 每个包发送前都会把 2 字节随机数展开后写满 IV（`iv_from_raw`）。

use sha3::{Digest, Sha3_256, Sha3_384};
use std::str::FromStr;

/// 加密模式，对应 C++ `enum class encryption_mode`。
///
/// 配置文件中的取值为：`none` / `xor` / `aes-gcm` / `aes-ocb` / `chacha20` / `xchacha20`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionMode {
    /// 未配置（C++ 的 `encryption_mode::empty`）
    Empty,
    /// 不加密，仅带 CRC32 校验
    None,
    /// 差分异或 + CRC32 校验
    PlainXor,
    /// AES-256-GCM，16 字节 IV
    AesGcm,
    /// AES-256-OCB，12 字节 IV
    AesOcb,
    /// ChaCha20-Poly1305，**8 字节 IV**（与 Botan 的 tag 构造尚未对齐）
    Chacha20,
    /// XChaCha20-Poly1305，24 字节 IV
    Xchacha20,
}

impl EncryptionMode {
    pub const fn is_aead(self) -> bool {
        matches!(
            self,
            EncryptionMode::AesGcm
                | EncryptionMode::AesOcb
                | EncryptionMode::Chacha20
                | EncryptionMode::Xchacha20
        )
    }

    /// 该模式的 IV 长度（字节）
    pub const fn iv_len(self) -> usize {
        match self {
            EncryptionMode::AesGcm => 16,
            EncryptionMode::AesOcb => 12,
            EncryptionMode::Chacha20 => 8,
            EncryptionMode::Xchacha20 => 24,
            _ => 0,
        }
    }
}

impl FromStr for EncryptionMode {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "none" => EncryptionMode::None,
            "xor" => EncryptionMode::PlainXor,
            "aes-gcm" => EncryptionMode::AesGcm,
            "aes-ocb" => EncryptionMode::AesOcb,
            "chacha20" => EncryptionMode::Chacha20,
            "xchacha20" => EncryptionMode::Xchacha20,
            "" => EncryptionMode::Empty,
            _ => return Err(()),
        })
    }
}

/// 派生出的密钥与初始 IV
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyIv {
    pub key: [u8; 32],
    pub iv: Vec<u8>,
}

/// `head_tail_xor`：把数据对半，前一半与「逆序的后一半」逐字节异或。
///
/// 与 C++ `encryption_base::head_tail_xor` 完全一致（返回长度 = 输入长度 / 2）。
pub fn head_tail_xor(input: &[u8]) -> Vec<u8> {
    let half = input.len() / 2;
    (0..half)
        .map(|i| input[i] ^ input[input.len() - 1 - i])
        .collect()
}

fn sha3_256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha3_256::new();
    hasher.update(data);
    let out = hasher.finalize();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out);
    key
}

fn sha3_384(data: &[u8]) -> [u8; 48] {
    let mut hasher = Sha3_384::new();
    hasher.update(data);
    let out = hasher.finalize();
    let mut key = [0u8; 48];
    key.copy_from_slice(&out);
    key
}

/// 重复折叠直到目标长度（对应 C++ 中的 `while (output.size() > iv.size())` 循环）
fn fold_to(mut data: Vec<u8>, target_len: usize) -> Vec<u8> {
    while data.len() > target_len {
        data = head_tail_xor(&data);
    }
    data
}

/// 依据密码与模式派生 key / 初始 IV。
///
/// 空密码时与 C++ 一致：key、iv 全为 0（长度仍按模式）。
pub fn derive_key_iv(mode: EncryptionMode, password: &str) -> KeyIv {
    let iv_len = mode.iv_len();
    if password.is_empty() || !mode.is_aead() {
        return KeyIv {
            key: [0u8; 32],
            iv: vec![0u8; iv_len],
        };
    }

    let password_bytes = password.as_bytes();
    let key = sha3_256(password_bytes);
    let iv = match mode {
        EncryptionMode::AesGcm => head_tail_xor(&key),
        EncryptionMode::AesOcb => fold_to(head_tail_xor(&sha3_384(password_bytes)), 12),
        EncryptionMode::Chacha20 => fold_to(head_tail_xor(&key), 8),
        EncryptionMode::Xchacha20 => head_tail_xor(&sha3_384(password_bytes)),
        _ => vec![0u8; iv_len],
    };

    KeyIv { key, iv }
}

/// 把 2 字节随机值展开为完整 IV（对应各模式的 `change_iv(iv_raw)`）。
pub fn iv_from_raw(mode: EncryptionMode, raw: u16) -> Vec<u8> {
    match mode {
        EncryptionMode::AesGcm => {
            let value = (raw as u64) << 48 | (raw as u64) << 32 | (raw as u64) << 16 | raw as u64;
            value.to_le_bytes().repeat(2)
        }
        EncryptionMode::AesOcb => {
            let value = ((raw as u32) << 16) | raw as u32;
            value.to_le_bytes().repeat(3)
        }
        EncryptionMode::Chacha20 | EncryptionMode::Xchacha20 => {
            let value = (raw as u64) << 48 | (raw as u64) << 32 | (raw as u64) << 16 | raw as u64;
            if mode == EncryptionMode::Chacha20 {
                value.to_le_bytes().to_vec()
            } else {
                value.to_le_bytes().repeat(3)
            }
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_tail_xor_matches_cpp() {
        // input = [0,1,2,3] -> [0^3, 1^2] = [3, 3]
        assert_eq!(head_tail_xor(&[0, 1, 2, 3]), vec![3, 3]);
        // 奇数长度时丢弃中间字节，与 C++ 的 len/2 一致
        assert_eq!(head_tail_xor(&[0, 1, 2, 3, 4]), vec![4, 2]);
    }

    #[test]
    fn derived_iv_lengths() {
        assert_eq!(derive_key_iv(EncryptionMode::AesGcm, "abc").iv.len(), 16);
        assert_eq!(derive_key_iv(EncryptionMode::AesOcb, "abc").iv.len(), 12);
        assert_eq!(derive_key_iv(EncryptionMode::Chacha20, "abc").iv.len(), 8);
        assert_eq!(derive_key_iv(EncryptionMode::Xchacha20, "abc").iv.len(), 24);
    }

    #[test]
    fn empty_password_gives_zero_key() {
        let key_iv = derive_key_iv(EncryptionMode::AesGcm, "");
        assert_eq!(key_iv.key, [0u8; 32]);
        assert_eq!(key_iv.iv, vec![0u8; 16]);
    }

    #[test]
    fn iv_from_raw_matches_cpp_change_iv() {
        // 0x0102 -> 展开为 16 位重复
        let iv = iv_from_raw(EncryptionMode::AesGcm, 0x0102);
        assert_eq!(iv.len(), 16);
        assert_eq!(&iv[..8], &0x0102_0102_0102_0102u64.to_le_bytes());
        assert_eq!(&iv[8..], &0x0102_0102_0102_0102u64.to_le_bytes());

        let iv = iv_from_raw(EncryptionMode::AesOcb, 0x0102);
        assert_eq!(iv, 0x0102_0102u32.to_le_bytes().repeat(3));

        assert_eq!(
            iv_from_raw(EncryptionMode::Chacha20, 0x0102),
            0x0102_0102_0102_0102u64.to_le_bytes()
        );
        assert_eq!(iv_from_raw(EncryptionMode::Xchacha20, 0x0102).len(), 24);
    }

    #[test]
    fn mode_parsing() {
        assert_eq!("aes-gcm".parse(), Ok(EncryptionMode::AesGcm));
        assert_eq!("xor".parse(), Ok(EncryptionMode::PlainXor));
        assert_eq!("bogus".parse::<EncryptionMode>(), Err(()));
    }
}
