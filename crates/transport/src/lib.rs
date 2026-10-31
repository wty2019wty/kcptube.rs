// SPDX-License-Identifier: GPL-3.0-only

//! 网络传输层。
//!
//! 对应 C++ 版 `networks/` 目录：`connections.cpp`（asio 封装）、
//! `kcp_updater.cpp`（KCP 定时驱动）、`mux_tunnel.cpp`（多路复用）。
//!
//! ## 与 asio 的映射
//!
//! | C++ | Rust |
//! |---|---|
//! | `asio::io_context`（light / heavy 两个） | 两个 `tokio` runtime，或一个多线程 runtime + 任务分组 |
//! | `asio::steady_timer` | `tokio::time::sleep` / `interval` |
//! | `shared_ptr<tcp_session>` 生命周期 | `Arc<Session>` + 显式关闭信号 |
//! | `asio::post` 跨线程投递 | `mpsc::channel` / `JoinHandle` |

use kcptube_kcp::{Kcp, KcpConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("会话已关闭")]
    Closed,
    #[error("功能尚未实现：{0}")]
    NotImplemented(&'static str),
}

/// KCP 会话 + 对端地址（对应 C++ `kcp_mappings`）
pub struct KcpEndpoint {
    pub kcp: Mutex<Kcp>,
    pub peer: SocketAddr,
}

impl KcpEndpoint {
    pub fn new(config: KcpConfig, peer: SocketAddr) -> Self {
        Self {
            kcp: Mutex::new(Kcp::new(config)),
            peer,
        }
    }
}

/// KCP 定时驱动（对应 C++ `KCPUpdater`）。
pub struct KcpUpdater {
    _private: (),
}

impl KcpUpdater {
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// 注册一个 KCP 会话，按 `kcp.check()` 的返回值安排下一次 update。
    pub async fn submit(&self, _endpoint: Arc<KcpEndpoint>) -> Result<(), TransportError> {
        Err(TransportError::NotImplemented("KcpUpdater::submit"))
    }
}

impl Default for KcpUpdater {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updater_can_be_created() {
        let _updater = KcpUpdater::new();
    }
}
