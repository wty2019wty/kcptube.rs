//! 中继模式（对应 C++ `modes/relay.cpp`，约 1800 行）。
//!
//! relay 由「入口（ingress）」与「出口（egress）」两段配置组成，
//! 用于在没有公网 IP 的场景下中转流量。

use kcptube_config::UserSettings;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("中继启动失败：{0}")]
    Startup(String),
    #[error("功能尚未实现：{0}")]
    NotImplemented(&'static str),
}

pub struct Relay {
    ingress: UserSettings,
    egress: UserSettings,
}

impl Relay {
    pub fn new(ingress: UserSettings, egress: UserSettings) -> Self {
        Self { ingress, egress }
    }

    pub fn ingress(&self) -> &UserSettings {
        &self.ingress
    }

    pub fn egress(&self) -> &UserSettings {
        &self.egress
    }

    /// 启动中继（阻塞直到退出）
    pub async fn run(&self) -> Result<(), RelayError> {
        Err(RelayError::NotImplemented("relay mode"))
    }
}
