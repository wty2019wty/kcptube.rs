// SPDX-License-Identifier: GPL-3.0-only

//! 加密帧的封装/解封装。
//!
//! 与 C++ `data_operations.cpp` 中 `encrypt_data` / `decrypt_data` 对应：
//!
//! ```text
//! AEAD 模式:    密文(含 AEAD tag) || iv_raw(2 字节 LE)
//! none/xor 模式: 数据 || crc_checksum(2 字节 LE)   [xor 另外做一次前向差分]
//! ```
//!
//! 每次加密都会重新随机生成 `iv_raw`，并与 C++ 的 `change_iv()` 使用相同展开规则。

use crate::ASSOCIATED_DATA;
use crate::derive::{EncryptionMode, derive_key_iv, iv_from_raw};
use aes::Aes256;
use aes_gcm::AesGcm;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::XChaCha20Poly1305;
use ocb3::Ocb3;
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
        EncryptionMode::Chacha20 => Err(CryptoError::Unsupported(
            mode,
            "8 字节 nonce 的 Poly1305 tag 构造与 Botan 不一致，尚待对齐",
        )),
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
        EncryptionMode::Chacha20 => Err(CryptoError::Unsupported(
            mode,
            "8 字节 nonce 的 Poly1305 tag 构造与 Botan 不一致，尚待对齐",
        )),
        _ => Err(CryptoError::Unsupported(mode, "非 AEAD 模式没有 AEAD 原语")),
    }
}

/// CRC32 折叠成 16 位校验（对应 C++ `simple_hashing::checksum16`）：
/// 低 16 位与高 16 位异或。
fn checksum16(data: &[u8]) -> [u8; 2] {
    let crc = crc32fast::hash(data);
    let low = (crc & 0xFFFF) as u16;
    let high = (crc >> 16) as u16;
    (low ^ high).to_le_bytes()
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
