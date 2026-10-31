//! 前向纠错（FEC）。
//!
//! C++ 版使用 Botan3 内置的 `Botan::ZFEC`（即 zfec 的移植）：
//! `fec(k, n)` 把 k 个原始块编码为 n 个 share，share 0..k 是原始数据切片，
//! share k..n 是冗余校验。
//!
//! kcptube 的做法（见 C++ `client.cpp::fec_maker`）：
//! 1. 先把 k 个数据包用 `fec_container`（2 字节长度前缀）拼接成一块；
//! 2. 编码后**只发送冗余 share**（原始 share 已各自单独发出）；
//! 3. 冗余包额外附带 `packet_layer_fec` 头（含 sn / sub_sn / kcp_conv）。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FecError {
    #[error("FEC 参数非法：k={k}, n={n}（要求 1 <= k <= n <= 256）")]
    InvalidParams { k: usize, n: usize },
    #[error("share 数量不足，无法恢复")]
    NotEnoughShares,
    #[error("功能尚未实现：{0}")]
    NotImplemented(&'static str),
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

/// ZFEC 编解码器。
#[derive(Debug)]
pub struct Zfec {
    config: FecConfig,
}

impl Zfec {
    pub fn new(config: FecConfig) -> Result<Self, FecError> {
        if config.is_enabled() && (config.original > config.total() || config.total() > 256) {
            return Err(FecError::InvalidParams {
                k: config.original,
                n: config.total(),
            });
        }
        Ok(Self { config })
    }

    pub fn config(&self) -> FecConfig {
        self.config
    }

    /// 编码：对每个 share 调用回调（share 索引, share 数据）。
    pub fn encode(
        &self,
        _data: &[u8],
        _on_share: impl FnMut(usize, &[u8]),
    ) -> Result<(), FecError> {
        Err(FecError::NotImplemented("ZFEC encode"))
    }

    /// 解码：传入已收集到的 share（按索引），返回原始数据。
    pub fn decode(&self, _shares: &[Option<Vec<u8>>]) -> Result<Vec<u8>, FecError> {
        Err(FecError::NotImplemented("ZFEC decode"))
    }
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
    }

    #[test]
    fn disabled_fec_is_allowed() {
        let codec = Zfec::new(FecConfig::disabled()).unwrap();
        assert!(!codec.config().is_enabled());
    }
}
