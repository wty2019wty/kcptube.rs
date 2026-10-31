//! 服务端模式（对应 C++ `modes/server.cpp`，约 1300 行）。
//!
//! 负责：
//! - UDP 监听、握手校验、token 分配
//! - 目标地址转发（TCP/UDP）、mux 会话表
//! - STUN 打洞（NAT 全锥型）
//! - DDNS 变更检测与外部脚本回调（`update_ipv4` / `update_ipv6`）

use kcptube_config::UserSettings;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("服务端启动失败：{0}")]
    Startup(String),
    #[error("功能尚未实现：{0}")]
    NotImplemented(&'static str),
}

pub struct Server {
    settings: UserSettings,
}

impl Server {
    pub fn new(settings: UserSettings) -> Self {
        Self { settings }
    }

    pub fn settings(&self) -> &UserSettings {
        &self.settings
    }

    /// 启动服务端（阻塞直到退出）
    pub async fn run(&self) -> Result<(), ServerError> {
        Err(ServerError::NotImplemented("server mode"))
    }
}
