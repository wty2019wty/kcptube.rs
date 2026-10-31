// SPDX-License-Identifier: GPL-3.0-only

//! 加密帧的封装/解封装。
//!
//! 与 C++ `data_operations.cpp` 中 `encrypt_data` / `decrypt_data` 对应：
//!
//! ```text
//! AEAD 模式:    密文(含 AEAD tag) || iv_raw(2 字节 LE)
//! none/xor 模式: 数据 || crc_checksum(2 字节 BE)   [xor 另外做一次前向差分]
//! ```
//!
//! 每次加密都会重新随机生成 `iv_raw`，并与 C++ 的 `change_iv()` 使用相同展开规则。

use crate::ASSOCIATED_DATA;
use crate::derive::{EncryptionMode, derive_key_iv, iv_from_raw};
use aes::Aes256;
use aes_gcm::AesGcm;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use chacha20::ChaCha20Legacy;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use chacha20poly1305::XChaCha20Poly1305;
use ocb3::Ocb3;
use poly1305::Poly1305;
use thiserror::Error;
use typenum::{U12, U16};

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("数据长度不足")]
    TooShort,
    #[error("AEAD 加解密失败（密钥/IV 不匹配或数据被篡改）")]
    AeadFailure,
    #[error("校验和不匹配")]
    ChecksumMismatch,
    #[error("加密模式 {0:?} 尚未实现：{1}")]
    Unsupported(EncryptionMode, &'static str),
}

/// Poly1305 一次性计算（原始版 AEAD 使用，不做 16 字节对齐补齐）。
fn poly1305_tag(key: &[u8; 32], data: &[u8]) -> [u8; 16] {
    let tag = Poly1305::new(key.into()).compute_unpadded(data);
    let mut output = [0u8; 16];
    output.copy_from_slice(&tag);
    output
}

/// Botan 对 8 字节 nonce 使用**原始版** ChaCha20-Poly1305（draft-agl），
/// 与 RFC 8439 的 12/24 字节 nonce（CFRG 版）差别只在 Poly1305 的编码方式：
///
/// - 原始版：`AAD || LE64(AAD 长度) || 密文 || LE64(密文长度)`，不做 16 字节补齐；
/// - CFRG 版：AAD 与密文各自补零到 16 字节边界，再追加两个 LE64 长度。
///
/// 流密码部分两者等价（8 字节 nonce 相当于 4 个 0 前缀），因此密文相同、tag 不同。
fn chacha20_legacy_tag(key: &[u8; 32], nonce: &[u8; 8], cipher_text: &[u8]) -> [u8; 16] {
    let mut stream = ChaCha20Legacy::new(key.into(), nonce.into());
    let mut first_block = [0u8; 64];
    stream.write_keystream(&mut first_block);

    let mut mac_data = Vec::with_capacity(ASSOCIATED_DATA.len() + cipher_text.len() + 16);
    mac_data.extend_from_slice(ASSOCIATED_DATA);
    mac_data.extend_from_slice(&(ASSOCIATED_DATA.len() as u64).to_le_bytes());
    mac_data.extend_from_slice(cipher_text);
    mac_data.extend_from_slice(&(cipher_text.len() as u64).to_le_bytes());

    let mut poly_key = [0u8; 32];
    poly_key.copy_from_slice(&first_block[..32]);
    poly1305_tag(&poly_key, &mac_data)
}

fn chacha20_legacy_encrypt(
    key: &[u8; 32],
    nonce: &[u8; 8],
    plain: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let mut stream = ChaCha20Legacy::new(key.into(), nonce.into());
    // counter 0 的 64 字节密钥流前 32 字节作为 Poly1305 的 key
    let mut first_block = [0u8; 64];
    stream.write_keystream(&mut first_block);

    let mut out = plain.to_vec();
    stream.apply_keystream(&mut out);

    out.extend_from_slice(&chacha20_legacy_tag(key, nonce, &out));
    Ok(out)
}

fn chacha20_legacy_decrypt(
    key: &[u8; 32],
    nonce: &[u8; 8],
    cipher_text: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if cipher_text.len() < 16 {
        return Err(CryptoError::TooShort);
    }
    let (body, tag) = cipher_text.split_at(cipher_text.len() - 16);

    if chacha20_legacy_tag(key, nonce, body).as_slice() != tag {
        return Err(CryptoError::AeadFailure);
    }

    let mut stream = ChaCha20Legacy::new(key.into(), nonce.into());
    let mut first_block = [0u8; 64];
    stream.write_keystream(&mut first_block);

    let mut out = body.to_vec();
    stream.apply_keystream(&mut out);
    Ok(out)
}

/// 直接对原始数据做一次 AEAD 加密（不追加 iv_raw），主要用于测试与内部复用。
///
/// 参数中的 `iv` 长度必须与模式匹配（见 `EncryptionMode::iv_len`）。
pub fn aead_encrypt_raw(
    mode: EncryptionMode,
    key: &[u8; 32],
    iv: &[u8],
    plain: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let payload = Payload {
        msg: plain,
        aad: ASSOCIATED_DATA,
    };

    match mode {
        EncryptionMode::AesGcm => {
            if iv.len() != 16 {
                return Err(CryptoError::TooShort);
            }
            let cipher = AesGcm::<Aes256, U16>::new(key.into());
            let nonce: &aes_gcm::Nonce<U16> = iv.try_into().map_err(|_| CryptoError::TooShort)?;
            cipher
                .encrypt(nonce, payload)
                .map_err(|_| CryptoError::AeadFailure)
        }
        EncryptionMode::AesOcb => {
            if iv.len() != 12 {
                return Err(CryptoError::TooShort);
            }
            let cipher = Ocb3::<Aes256, U12>::new(key.into());
            let nonce: &ocb3::Nonce<U12> = iv.try_into().map_err(|_| CryptoError::TooShort)?;
            cipher
                .encrypt(nonce, payload)
                .map_err(|_| CryptoError::AeadFailure)
        }
        EncryptionMode::Xchacha20 => {
            if iv.len() != 24 {
                return Err(CryptoError::TooShort);
            }
            let cipher = XChaCha20Poly1305::new(key.into());
            let nonce: &chacha20poly1305::XNonce =
                iv.try_into().map_err(|_| CryptoError::TooShort)?;
            cipher
                .encrypt(nonce, payload)
                .map_err(|_| CryptoError::AeadFailure)
        }
        EncryptionMode::Chacha20 => {
            let nonce: &[u8; 8] = iv.try_into().map_err(|_| CryptoError::TooShort)?;
            chacha20_legacy_encrypt(key, nonce, plain)
        }
        _ => Err(CryptoError::Unsupported(mode, "非 AEAD 模式没有 AEAD 原语")),
    }
}

/// 对原始数据做一次 AEAD 解密（不解析 iv_raw 后缀）
pub fn aead_decrypt_raw(
    mode: EncryptionMode,
    key: &[u8; 32],
    iv: &[u8],
    cipher_text: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let payload = Payload {
        msg: cipher_text,
        aad: ASSOCIATED_DATA,
    };

    match mode {
        EncryptionMode::AesGcm => {
            if iv.len() != 16 {
                return Err(CryptoError::TooShort);
            }
            let cipher = AesGcm::<Aes256, U16>::new(key.into());
            let nonce: &aes_gcm::Nonce<U16> = iv.try_into().map_err(|_| CryptoError::TooShort)?;
            cipher
                .decrypt(nonce, payload)
                .map_err(|_| CryptoError::AeadFailure)
        }
        EncryptionMode::AesOcb => {
            if iv.len() != 12 {
                return Err(CryptoError::TooShort);
            }
            let cipher = Ocb3::<Aes256, U12>::new(key.into());
            let nonce: &ocb3::Nonce<U12> = iv.try_into().map_err(|_| CryptoError::TooShort)?;
            cipher
                .decrypt(nonce, payload)
                .map_err(|_| CryptoError::AeadFailure)
        }
        EncryptionMode::Xchacha20 => {
            if iv.len() != 24 {
                return Err(CryptoError::TooShort);
            }
            let cipher = XChaCha20Poly1305::new(key.into());
            let nonce: &chacha20poly1305::XNonce =
                iv.try_into().map_err(|_| CryptoError::TooShort)?;
            cipher
                .decrypt(nonce, payload)
                .map_err(|_| CryptoError::AeadFailure)
        }
        EncryptionMode::Chacha20 => {
            let nonce: &[u8; 8] = iv.try_into().map_err(|_| CryptoError::TooShort)?;
            chacha20_legacy_decrypt(key, nonce, cipher_text)
        }
        _ => Err(CryptoError::Unsupported(mode, "非 AEAD 模式没有 AEAD 原语")),
    }
}

/// CRC32 折叠成 16 位校验（对应 C++ `simple_hashing::checksum16`）：
/// 低 16 位与高 16 位异或。
///
/// 注意字节序：Botan 的 `CRC32::final_result` 用 `store_be` 输出，
/// C++ 再把前两个字节/后两个字节各按本机 `uint16_t` 读入并异或，
/// 结果写回时仍是**大端**。早期实现误用了小端，遇到高低半字异或结果
/// 不对称时会与 C++ 不一致。
fn checksum16(data: &[u8]) -> [u8; 2] {
    let crc = crc32fast::hash(data);
    let low = (crc & 0xFFFF) as u16;
    let high = (crc >> 16) as u16;
    (low ^ high).to_be_bytes()
}

/// 前向差分异或（对应 C++ `xor_forward`）
fn xor_forward(data: &mut [u8]) {
    if data.len() < 2 {
        return;
    }
    for i in 0..data.len() - 1 {
        data[i] ^= data[i + 1];
    }
}

/// 前向差分异或的逆运算（对应 C++ `xor_backward`）
fn xor_backward(data: &mut [u8]) {
    if data.len() < 2 {
        return;
    }
    for i in (1..data.len()).rev() {
        data[i - 1] ^= data[i];
    }
}

/// 持有密钥与模式的加密器（对应 C++ `encryption_base` 系列）。
pub struct Cipher {
    mode: EncryptionMode,
    key: [u8; 32],
}

impl Cipher {
    /// 依据密码创建加密器
    pub fn new(mode: EncryptionMode, password: &str) -> Self {
        let key_iv = derive_key_iv(mode, password);
        Self {
            mode,
            key: key_iv.key,
        }
    }

    pub fn mode(&self) -> EncryptionMode {
        self.mode
    }

    pub fn key(&self) -> &[u8; 32] {
        &self.key
    }

    /// 加密一段数据（自动生成 iv_raw 并追加在尾部）
    pub fn encrypt(&self, plain: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match self.mode {
            EncryptionMode::Empty | EncryptionMode::None | EncryptionMode::PlainXor => {
                let mut out = plain.to_vec();
                out.extend_from_slice(&checksum16(plain));
                if self.mode == EncryptionMode::PlainXor {
                    xor_forward(&mut out);
                }
                Ok(out)
            }
            mode => {
                let raw: u16 = rand::random();
                let iv = iv_from_raw(mode, raw);
                let mut out = aead_encrypt_raw(mode, &self.key, &iv, plain)?;
                out.extend_from_slice(&raw.to_le_bytes());
                Ok(out)
            }
        }
    }

    /// 解密（解析尾部的 iv_raw / 校验和）
    pub fn decrypt(&self, cipher_text: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match self.mode {
            EncryptionMode::Empty | EncryptionMode::None | EncryptionMode::PlainXor => {
                let mut data = cipher_text.to_vec();
                if self.mode == EncryptionMode::PlainXor {
                    xor_backward(&mut data);
                }
                if data.len() < 2 {
                    return Err(CryptoError::TooShort);
                }
                let (body, tail) = data.split_at(data.len() - 2);
                if checksum16(body) != tail {
                    return Err(CryptoError::ChecksumMismatch);
                }
                Ok(body.to_vec())
            }
            mode => {
                if cipher_text.len() < 2 {
                    return Err(CryptoError::TooShort);
                }
                let (body, raw) = cipher_text.split_at(cipher_text.len() - 2);
                let raw = u16::from_le_bytes([raw[0], raw[1]]);
                let iv = iv_from_raw(mode, raw);
                aead_decrypt_raw(mode, &self.key, &iv, body)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 与 C++ Botan 用同一组向量对撞得到的期望值（2026-10 实测，AAD = "KCP PortHopping"）。
    /// key = [0x01; 32]，iv = [0x02; N]，plain = "hello kcptube!!"
    const KEY: [u8; 32] = [1u8; 32];
    const PLAIN: &[u8] = b"hello kcptube!!";

    #[test]
    fn vectors_match_botan() {
        let cases: [(EncryptionMode, Vec<u8>, &str); 3] = [
            (
                EncryptionMode::AesGcm,
                vec![2u8; 16],
                "1853FB26B0E14477AE58077EC07BF2E883F8C2F2DFB2BDF271268B68F21A62",
            ),
            (
                EncryptionMode::AesOcb,
                vec![2u8; 12],
                "C581B50BAB10C402FBB3CBA947AAE79AEAD48A1A0EAB1B2EDCC0D020184D04",
            ),
            (
                EncryptionMode::Xchacha20,
                vec![2u8; 24],
                "A4035A83C557972ED01514F31771587E41681F34FAC50AFAE603C81775BD05",
            ),
        ];

        for (mode, iv, expected) in cases {
            let out = aead_encrypt_raw(mode, &KEY, &iv, PLAIN).unwrap();
            assert_eq!(hex::encode_upper(&out), expected, "mode = {mode:?}");
        }
    }

    #[test]
    fn chacha20_8byte_nonce_matches_botan() {
        // 由 Botan 3.13 CLI 生成：
        // botan cipher --cipher=ChaCha20Poly1305 --key=[0x01;32] \
        //   --nonce=[0x02;8] --ad=$(hex "KCP PortHopping") plain.bin
        // 注意这是**原始版**（draft-agl）ChaCha20-Poly1305，tag 编码与 RFC 8439 不同。
        let out = aead_encrypt_raw(EncryptionMode::Chacha20, &KEY, &[2u8; 8], PLAIN).unwrap();
        assert_eq!(
            hex::encode_upper(&out),
            "C80B52B63083322CD6C4169457B3C24ACFDD51C9A9C845910F8DFC7F90AD1A"
        );
    }

    /// Botan `CRC32::final_result` 使用 `store_be`，因此 checksum16 也要按大端输出。
    #[test]
    fn checksum16_matches_botan_crc32() {
        assert_eq!(checksum16(PLAIN), [0x86, 0x5D]);
        let cipher = Cipher::new(EncryptionMode::None, "");
        let sealed = cipher.encrypt(PLAIN).unwrap();
        assert_eq!(&sealed[sealed.len() - 2..], &[0x86, 0x5D]);
    }

    /// 直接用 aad = ASSOCIATED_DATA 的向量做往返测试（自洽性）
    #[test]
    fn aead_roundtrip() {
        for mode in [
            EncryptionMode::AesGcm,
            EncryptionMode::AesOcb,
            EncryptionMode::Xchacha20,
        ] {
            let cipher = Cipher::new(mode, "test-password");
            let sealed = cipher.encrypt(PLAIN).unwrap();
            assert!(sealed.len() > PLAIN.len());
            let opened = cipher.decrypt(&sealed).unwrap();
            assert_eq!(opened, PLAIN);
        }
    }

    #[test]
    fn tampered_cipher_text_fails() {
        let cipher = Cipher::new(EncryptionMode::AesGcm, "test-password");
        let mut sealed = cipher.encrypt(PLAIN).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xFF;
        assert!(cipher.decrypt(&sealed).is_err());
    }

    #[test]
    fn none_and_xor_roundtrip() {
        for mode in [EncryptionMode::None, EncryptionMode::PlainXor] {
            let cipher = Cipher::new(mode, "");
            let sealed = cipher.encrypt(PLAIN).unwrap();
            assert_eq!(sealed.len(), PLAIN.len() + 2);
            assert_eq!(cipher.decrypt(&sealed).unwrap(), PLAIN);
        }
    }

    #[test]
    fn xor_forward_is_invertible() {
        let mut data = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let original = data.clone();
        xor_forward(&mut data);
        assert_ne!(data, original);
        xor_backward(&mut data);
        assert_eq!(data, original);
    }
}
