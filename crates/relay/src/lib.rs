// SPDX-License-Identifier: GPL-3.0-only

//! 中继模式（对应 C++ `modes/relay.cpp`）。
//!
//! relay 由「入口（ingress）」与「出口（egress）」两段配置组成，用于在没有
//! 公网 IP 的场景下中转流量。它只监听 **UDP**，在两条 KCP 通道之间透明转发
//! 内层帧，不落地 TCP / UDP 连接：
//!
//! ```text
//! client ---KCP(A)--- relay ---KCP(B)--- server
//!        ingress 段          egress 段
//! ```
//!
//! 模块划分：
//!
//! | 文件 | 职责 |
//! |---|---|
//! | `listener.rs` | ingress 监听、按 `conv` 分派 |
//! | `handshake.rs` | 两段之间的 `initialise` 改写与会话建立 |
//! | `session.rs` | 两条 KCP 通道之间的数据转发 |
//! | `state.rs` | 握手表、会话表与共享状态 |
//!
//! 本阶段（R1）只包含握手与数据转发；空闲清理、保活发送、状态日志与端口
//! 跳跃属于 R2/R3。

mod handshake;
mod listener;
mod maintenance;
mod port_hop;
mod session;
mod state;
mod status;
mod util;

use crate::listener::run_listener;
use crate::state::RelayState;
use kcptube_config::UserSettings;
use kcptube_protocol::APP_NAME;
use kcptube_protocol::ProtocolError;
use kcptube_transport::{TransportError, UdpEndpoint};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("中继启动失败：{0}")]
    Startup(String),
    #[error("传输层错误：{0}")]
    Transport(#[from] TransportError),
    #[error("协议层错误：{0}")]
    Protocol(#[from] ProtocolError),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
}

pub struct Relay {
    settings: UserSettings,
}

impl Relay {
    /// `settings` 是**根配置**：`[listener]` / `[forwarder]` 两段在内部展开。
    pub fn new(settings: UserSettings) -> Self {
        Self { settings }
    }

    /// 启动中继（阻塞直到所有监听任务退出）。
    pub async fn run(&self) -> Result<(), RelayError> {
        println!("{APP_NAME} running in relay mode");

        let state = Arc::new(RelayState::new(Arc::new(self.settings.clone()))?);
        let addresses = util::listen_addresses(&state.ingress);
        if addresses.is_empty() {
            return Err(RelayError::Startup("listen_port is not set".to_string()));
        }

        // 维护任务：空闲回收 + 两侧保活 + 状态日志
        tokio::spawn(maintenance::run_maintenance(state.clone()));

        let mut tasks = tokio::task::JoinSet::new();
        for address in addresses {
            let socket = UdpEndpoint::bind(address)
                .await
                .map_err(|error| RelayError::Startup(format!("无法监听 {address}：{error}")))?;
            let state = state.clone();
            tasks.spawn(async move { run_listener(state, socket).await });
        }

        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!("relay 监听任务结束：{error}"),
                Err(error) => tracing::warn!("relay 监听任务异常：{error}"),
            }
        }
        Ok(())
    }
}
