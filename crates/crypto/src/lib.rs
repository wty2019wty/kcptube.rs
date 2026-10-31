// SPDX-License-Identifier: GPL-3.0-only

//! kcptube 加密层。
//!
//! 目标：与 C++ 版（Botan3）**逐字节**兼容。已用同一组向量验证过的模式：
//! `aes-gcm`、`aes-ocb`、`chacha20`、`xchacha20`（4 种 AEAD），
//! 以及 `xor` / `none` 的 CRC32 校验。
//!
//! `chacha20` 使用 **8 字节 IV**，对应 Botan 的**原始版** ChaCha20-Poly1305
//! （draft-agl），与 RFC 8439 的差别仅在于 Poly1305 的长度编码；
//! 已由 `cipher.rs::chacha20_8byte_nonce_matches_botan` 与 Botan 逐字节比对。
//!
//! 帧格式（对应 C++ `data_operations.cpp`）：
//!
//! ```text
//! 加密：密文 || iv_raw(2 字节，小端)
//! 解密：读末尾 2 字节 iv_raw -> 还原 IV -> AEAD 解密
//! ```
//!
//! `xor`（`plain_xor`）模式：数据 || crc32_checksum16(2 字节)，再做前向差分异或。

pub mod cipher;
pub mod derive;

pub use cipher::{Cipher, CryptoError, aead_encrypt_raw};
pub use derive::{EncryptionMode, KeyIv, derive_key_iv, head_tail_xor};

/// AEAD 的附加认证数据，与 C++ 中 `encryption_base::associated_data` 一致
pub const ASSOCIATED_DATA: &[u8] = b"KCP PortHopping";
