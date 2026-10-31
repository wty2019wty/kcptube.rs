//! 客户端模式（对应 C++ `modes/client.cpp`，约 1900 行）。
//!
//! 负责：
//! - 本地 TCP/UDP 监听，接受用户连接
//! - 与远端完成握手（`initialise` / `keep_alive` / `raw_data`）
//! - 端口跳跃（`dport_refresh` + `test_connection` 探测）
//! - mux 多路复用、限速、状态日志

use kcptube_config::UserSettings;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("客户端启动失败：{0}")]
    Startup(String),
    #[error("功能尚未实现：{0}")]
    NotImplemented(&'static str),
}

pub struct Client {
    settings: UserSettings,
}

impl Client {
    pub fn new(settings: UserSettings) -> Self {
        Self { settings }
    }

    pub fn settings(&self) -> &UserSettings {
        &self.settings
    }

    /// 启动客户端（阻塞直到退出）
    pub async fn run(&self) -> Result<(), ClientError> {
        Err(ClientError::NotImplemented("client mode"))
    }
}
