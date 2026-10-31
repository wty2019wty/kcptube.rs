// SPDX-License-Identifier: GPL-3.0-only

//! 前向纠错（FEC）。
//!
//! C++ 版使用 Botan3 内置的 `Botan::ZFEC`（Luigi Rizzo 的 Vandermonde 矩阵方案，
//! 与 tahoe-lafs/zfec 兼容）：`fec(k, n)` 把 k 个**等长**原始块编码为 n 个 share，
//! share 0..k 是原始数据切片，share k..n 是冗余校验。
//!
//! kcptube 的做法（见 C++ `client.cpp::fec_maker`）：
//! 1. 先把 k 个数据包用 `fec_container`（2 字节长度前缀）拼接成等长块；
//! 2. 编码后**只发送冗余 share**（原始 share 已各自单独发出）；
//! 3. 冗余包额外附带 `packet_layer_fec` 头（含 sn / sub_sn / kcp_conv）。
//!
//! 本实现逐行对应 `legacy-cpp` 依赖的 Botan `zfec.cpp`，
//! 包括 `create_inverted_vdm` 的转置存储方式，确保 share 字节完全一致。

use std::sync::OnceLock;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FecError {
    #[error("FEC 参数非法：k={k}, n={n}（要求 1 <= k <= n < 256）")]
    InvalidParams { k: usize, n: usize },
    #[error("数据长度 {length} 不是 k={k} 的整数倍")]
    MisalignedInput { length: usize, k: usize },
    #[error("share 数量不足，无法恢复")]
    NotEnoughShares,
    #[error("share 长度不一致")]
    ShareSizeMismatch,
    #[error("fec_container 数据长度不足")]
    ContainerTooShort,
}

/// FEC 配置：`original` 个原始包 + `redundant` 个冗余包。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecConfig {
    pub original: usize,
    pub redundant: usize,
}

impl FecConfig {
    pub const fn disabled() -> Self {
        Self {
            original: 0,
            redundant: 0,
        }
    }

    pub const fn is_enabled(&self) -> bool {
        self.original > 0 && self.redundant > 0
    }

    pub const fn total(&self) -> usize {
        self.original + self.redundant
    }
}

// ---- GF(2^8)：多项式 x^8 + x^4 + x^3 + x^2 + 1 (0x11D) ----

struct GfTables {
    exp: [u8; 255],
    log: [u8; 256],
    inverse: [u8; 256],
}

fn gf_tables() -> &'static GfTables {
    static TABLES: OnceLock<GfTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut exp = [0u8; 255];
        let mut log = [0u8; 256];
        let mut value: u16 = 1;
        for (index, slot) in exp.iter_mut().enumerate() {
            *slot = value as u8;
            log[value as usize] = index as u8;
            value <<= 1;
            if value & 0x100 != 0 {
                value ^= 0x11D;
            }
        }

        let mut inverse = [0u8; 256];
        for index in 0..255 {
            // exp[i] * exp[255 - i] = 1
            inverse[exp[index] as usize] = exp[(255 - index) % 255];
        }

        GfTables { exp, log, inverse }
    })
}

fn gf_mul(left: u8, right: u8) -> u8 {
    if left == 0 || right == 0 {
        return 0;
    }
    let tables = gf_tables();
    let sum = u16::from(tables.log[left as usize]) + u16::from(tables.log[right as usize]);
    tables.exp[(sum % 255) as usize]
}

/// 对应 Botan `create_inverted_vdm`（注意其按列写入 vdm）
fn create_inverted_vdm(vdm: &mut [u8], k: usize) {
    if k == 0 {
        return;
    }
    if k == 1 {
        vdm[0] = 1;
        return;
    }

    let tables = gf_tables();
    let mut b = vec![0u8; k];
    let mut c = vec![0u8; k];

    c[k - 1] = 0;
    for i in 1..k {
        let p = tables.exp[i];
        for j in (k - 1 - (i - 1))..(k - 1) {
            if j + 1 < k {
                c[j] ^= gf_mul(p, c[j + 1]);
            }
        }
        c[k - 1] ^= tables.exp[i];
    }

    for row in 0..k {
        let p = if row == 0 { 0 } else { tables.exp[row] };
        let mut t = 1u8;
        b[k - 1] = 1;
        for i in (1..k).rev() {
            b[i - 1] = c[i] ^ gf_mul(p, b[i]);
            t = b[i - 1] ^ gf_mul(p, t);
        }
        let t_inv = tables.inverse[t as usize];
        for col in 0..k {
            vdm[col * k + row] = gf_mul(t_inv, b[col]);
        }
    }
}

/// 对应 Botan `invert_matrix`：对 k×k 矩阵做 GF(2^8) 上的高斯-约当消元。
fn invert_matrix(matrix: &mut [u8], k: usize) -> Result<(), FecError> {
    let mut inverse = vec![0u8; k * k];
    for i in 0..k {
        inverse[i * k + i] = 1;
    }
    let tables = gf_tables();

    for col in 0..k {
        let pivot = (col..k).find(|row| matrix[row * k + col] != 0);
        let Some(pivot) = pivot else {
            return Err(FecError::NotEnoughShares);
        };

        if pivot != col {
            for i in 0..k {
                matrix.swap(col * k + i, pivot * k + i);
                inverse.swap(col * k + i, pivot * k + i);
            }
        }

        let pivot_inverse = tables.inverse[matrix[col * k + col] as usize];
        for i in 0..k {
            matrix[col * k + i] = gf_mul(matrix[col * k + i], pivot_inverse);
            inverse[col * k + i] = gf_mul(inverse[col * k + i], pivot_inverse);
        }

        for row in 0..k {
            if row == col {
                continue;
            }
            let factor = matrix[row * k + col];
            if factor == 0 {
                continue;
            }
            for i in 0..k {
                let subtrahend = gf_mul(factor, matrix[col * k + i]);
                matrix[row * k + i] ^= subtrahend;
                let subtrahend = gf_mul(factor, inverse[col * k + i]);
                inverse[row * k + i] ^= subtrahend;
            }
        }
    }

    matrix.copy_from_slice(&inverse);
    Ok(())
}

/// ZFEC 编解码器。
#[derive(Debug)]
pub struct Zfec {
    config: FecConfig,
    /// n × k 编码矩阵（行优先）
    enc_matrix: Vec<u8>,
}

impl Zfec {
    pub fn new(config: FecConfig) -> Result<Self, FecError> {
        let k = config.original;
        let n = config.total();
        if !config.is_enabled() {
            return Ok(Self {
                config,
                enc_matrix: Vec::new(),
            });
        }
        if k == 0 || n == 0 || k >= 256 || n >= 256 || k > n {
            return Err(FecError::InvalidParams { k, n });
        }

        let mut temp = vec![0u8; n * k];
        create_inverted_vdm(&mut temp, k);
        let tables = gf_tables();
        for (offset, slot) in temp[k * k..].iter_mut().enumerate() {
            let index = k * k + offset;
            let exponent = ((index / k) * (index % k)) % 255;
            *slot = tables.exp[exponent];
        }

        let mut enc_matrix = vec![0u8; n * k];
        for i in 0..k {
            enc_matrix[i * (k + 1)] = 1;
        }
        for row in k..n {
            for col in 0..k {
                let mut acc = 0u8;
                for i in 0..k {
                    let row_v = temp[row * k + i];
                    let row_c = temp[col + k * i];
                    acc ^= gf_mul(row_v, row_c);
                }
                enc_matrix[row * k + col] = acc;
            }
        }

        Ok(Self { config, enc_matrix })
    }

    pub fn config(&self) -> FecConfig {
        self.config
    }

    pub fn recovery_threshold(&self) -> usize {
        self.config.original
    }

    pub fn generated_shares(&self) -> usize {
        self.config.total()
    }

    fn linear_combination(&self, shares: &[&[u8]], coefficients: &[u8], output: &mut [u8]) {
        output.fill(0);
        for (share, coefficient) in shares.iter().zip(coefficients) {
            if *coefficient == 0 {
                continue;
            }
            for (slot, byte) in output.iter_mut().zip(share.iter()) {
                *slot ^= gf_mul(*coefficient, *byte);
            }
        }
    }

    /// 编码：对每个 share 调用回调（share 索引, share 数据）。
    ///
    /// 输入长度必须是 `k` 的整数倍，每个 share 长度为 `input.len() / k`。
    pub fn encode(
        &self,
        data: &[u8],
        mut on_share: impl FnMut(usize, &[u8]),
    ) -> Result<(), FecError> {
        let k = self.config.original;
        if k == 0 || data.len() % k != 0 {
            return Err(FecError::MisalignedInput {
                length: data.len(),
                k,
            });
        }
        let share_size = data.len() / k;
        let shares: Vec<&[u8]> = (0..k)
            .map(|index| &data[index * share_size..(index + 1) * share_size])
            .collect();

        for (index, share) in shares.iter().enumerate() {
            on_share(index, share);
        }

        let mut buffer = vec![0u8; share_size];
        for index in k..self.config.total() {
            self.linear_combination(
                &shares,
                &self.enc_matrix[index * k..(index + 1) * k],
                &mut buffer,
            );
            on_share(index, &buffer);
        }
        Ok(())
    }

    /// 解码：传入按 share 索引排列的数据（缺失为 `None`），返回原始 k 个 share 的拼接。
    pub fn decode(&self, shares: &[Option<Vec<u8>>]) -> Result<Vec<u8>, FecError> {
        let k = self.config.original;
        let n = self.config.total();

        let mut available = Vec::with_capacity(k);
        let mut share_size = None;
        for (index, share) in shares.iter().enumerate().take(n) {
            let Some(data) = share else {
                continue;
            };
            if let Some(expected) = share_size {
                if data.len() != expected {
                    return Err(FecError::ShareSizeMismatch);
                }
            } else {
                share_size = Some(data.len());
            }
            available.push((index, data.as_slice()));
            if available.len() == k {
                break;
            }
        }

        if available.len() < k {
            return Err(FecError::NotEnoughShares);
        }
        let share_size = share_size.unwrap_or(0);

        let mut decoding_matrix = vec![0u8; k * k];
        for (slot, (index, _)) in available.iter().enumerate() {
            if *index < k {
                // share index 小于 k 时就是原始块，对应 k×k 单位矩阵的第 index 列
                decoding_matrix[slot * k + index] = 1;
            } else {
                decoding_matrix[slot * k..(slot + 1) * k]
                    .copy_from_slice(&self.enc_matrix[index * k..(index + 1) * k]);
            }
        }
        invert_matrix(&mut decoding_matrix, k)?;

        let share_refs: Vec<&[u8]> = available.iter().map(|(_, data)| *data).collect();
        let mut output = vec![0u8; k * share_size];
        let mut buffer = vec![0u8; share_size];
        for row in 0..k {
            self.linear_combination(
                &share_refs,
                &decoding_matrix[row * k..(row + 1) * k],
                &mut buffer,
            );
            output[row * share_size..(row + 1) * share_size].copy_from_slice(&buffer);
        }
        Ok(output)
    }
}

/// `fec_container` 的头部长度：2 字节大端数据长度
pub const FEC_CONTAINER_HEADER: usize = 2;

/// 对应 C++ `compact_into_container`（发送端）：
/// 把 k 个包按「最大长度 + 2」对齐并拼接，每个槽位写入 `u16 BE` 长度前缀。
///
/// 返回 `(拼接后的数据, 对齐长度)`。
pub fn pack_containers(items: &[&[u8]]) -> (Vec<u8>, usize) {
    let max_size = items.iter().map(|item| item.len()).max().unwrap_or(0);
    let align_length = max_size + FEC_CONTAINER_HEADER;
    let mut output = vec![0u8; items.len() * align_length];

    for (index, item) in items.iter().enumerate() {
        let start = index * align_length;
        let length = u16::try_from(item.len()).unwrap_or(u16::MAX);
        output[start..start + FEC_CONTAINER_HEADER].copy_from_slice(&length.to_be_bytes());
        output[start + FEC_CONTAINER_HEADER..start + FEC_CONTAINER_HEADER + item.len()]
            .copy_from_slice(item);
    }

    (output, align_length)
}

/// 对应 C++ `extract_from_container` / `copy_from_container`：
/// 读取一个槽位的长度前缀并取出原始数据。
pub fn unpack_container(container: &[u8]) -> Result<&[u8], FecError> {
    if container.len() < FEC_CONTAINER_HEADER {
        return Err(FecError::ContainerTooShort);
    }
    let length = usize::from(u16::from_be_bytes([container[0], container[1]]));
    let end = FEC_CONTAINER_HEADER
        .checked_add(length)
        .ok_or(FecError::ContainerTooShort)?;
    container
        .get(FEC_CONTAINER_HEADER..end)
        .ok_or(FecError::ContainerTooShort)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_params_rejected() {
        let codec = Zfec::new(FecConfig {
            original: 300,
            redundant: 10,
        });
        assert!(codec.is_err());

        assert!(
            Zfec::new(FecConfig {
                original: 4,
                redundant: 2
            })
            .is_ok()
        );
    }

    #[test]
    fn disabled_fec_is_allowed() {
        let codec = Zfec::new(FecConfig::disabled()).unwrap();
        assert!(!codec.config().is_enabled());
    }

    #[test]
    fn gf_tables_are_consistent() {
        let tables = gf_tables();
        for i in 0..255 {
            assert_eq!(
                gf_mul(tables.exp[i], tables.inverse[tables.exp[i] as usize]),
                1
            );
        }
        assert_eq!(gf_mul(0x02, 0x80), 0x1D); // 0x11D 生成多项式：2 * 0x80 = 0x1D
    }

    /// 向量由 `legacy-cpp` 依赖的 Botan 3.13 `Botan::ZFEC(4,6)` 生成：
    /// 输入 `00 01 .. 0F`，share 4 与 share 5 分别为 `88..8B`、`24..27`。
    #[test]
    fn vectors_match_botan_zfec() {
        let codec = Zfec::new(FecConfig {
            original: 4,
            redundant: 2,
        })
        .unwrap();
        let data: Vec<u8> = (0u8..16).collect();
        let mut shares = vec![Vec::new(); 6];
        codec
            .encode(&data, |index, share| shares[index] = share.to_vec())
            .unwrap();
        assert_eq!(shares[0], vec![0x00, 0x01, 0x02, 0x03]);
        assert_eq!(shares[3], vec![0x0C, 0x0D, 0x0E, 0x0F]);
        assert_eq!(shares[4], vec![0x88, 0x89, 0x8A, 0x8B]);
        assert_eq!(shares[5], vec![0x24, 0x25, 0x26, 0x27]);
    }

    #[test]
    fn encode_then_decode_roundtrip() {
        let codec = Zfec::new(FecConfig {
            original: 4,
            redundant: 2,
        })
        .unwrap();
        let data: Vec<u8> = (0u8..16).collect();
        let mut shares = vec![None; 6];
        codec
            .encode(&data, |index, share| {
                shares[index] = Some(share.to_vec());
            })
            .unwrap();

        assert_eq!(shares[0], Some(vec![0, 1, 2, 3]));
        assert_eq!(shares[3], Some(vec![12, 13, 14, 15]));

        // 丢掉两个原始 share，只留 k 个（含冗余）
        let mut received = shares.clone();
        received[0] = None;
        received[2] = None;
        assert_eq!(codec.decode(&received).unwrap(), data);
    }

    #[test]
    fn container_roundtrip() {
        let first = b"abc".to_vec();
        let second = b"de".to_vec();
        let (packed, align) = pack_containers(&[&first, &second]);
        assert_eq!(align, 5);
        assert_eq!(packed.len(), 10);
        assert_eq!(&packed[..2], &[0, 3]);
        assert_eq!(unpack_container(&packed[..align]).unwrap(), b"abc");
        assert_eq!(unpack_container(&packed[align..]).unwrap(), b"de");
        assert_eq!(
            unpack_container(&[0, 9, 1]),
            Err(FecError::ContainerTooShort)
        );
    }

    #[test]
    fn not_enough_shares() {
        let codec = Zfec::new(FecConfig {
            original: 3,
            redundant: 2,
        })
        .unwrap();
        let data = vec![7u8; 12];
        let mut shares = vec![None; 5];
        codec
            .encode(&data, |index, share| shares[index] = Some(share.to_vec()))
            .unwrap();
        shares[1] = None;
        shares[3] = None;
        shares[4] = None;
        assert_eq!(codec.decode(&shares), Err(FecError::NotEnoughShares));
    }
}
