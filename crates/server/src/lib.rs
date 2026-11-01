// SPDX-License-Identifier: GPL-3.0-only

//! 服务端模式（对应 C++ `modes/server.cpp`）。
//!
//! 已实现：UDP 端口范围监听、`initialise` 握手、TCP/UDP/mux 转发、FEC、
//! 端口跳跃（服务端侧按源地址/监听 socket 重定向）、空闲清理、保活与状态日志。
//! `ignore_destination_*` 目标与 mux `pre_connect_custom_address` 已接入；
//! STUN/DDNS 属于 M6。
//!
//! 模块划分：
//! - [`state`]：握手表、数据会话表与共享状态
//! - [`socket`]：监听收包与全局驱动
//! - [`handshake`]：握手与 TCP/UDP/mux 会话建立
//! - [`data`]：数据帧处理、KCP 驱动与空闲维护
//! - [`relay`]：本地目标 → KCP 的读取任务
//! - [`mux`]：mux 事件循环与本地目标连接
//! - [`status`]：状态摘要日志
//! - [`util`]：KCP 配置与地址/端口检查

mod data;
mod handshake;
mod mux;
mod relay;
mod socket;
mod state;
mod status;
mod util;

use crate::state::SocketState;
use kcptube_config::UserSettings;
use kcptube_kcp::KcpError;
use kcptube_logging::{print_message_to_file, time_string_with_brackets};
use kcptube_protocol::ProtocolError;
use kcptube_transport::{PacketCodec, TransportError, UdpBindOptions, UdpEndpoint};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use thiserror::Error;
use tokio::task::JoinSet;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("服务端启动失败：{0}")]
    Startup(String),
    #[error("传输层错误：{0}")]
    Transport(#[from] TransportError),
    #[error("协议错误：{0}")]
    Protocol(#[from] ProtocolError),
    #[error("KCP 错误：{0}")]
    Kcp(#[from] KcpError),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
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
        let settings = Arc::new(self.settings.clone());
        let codec = Arc::new(PacketCodec::new(
            settings.encryption,
            &settings.encryption_password,
        ));

        if settings.listen_ports.is_empty() {
            return Err(ServerError::Startup("listen_port is not set".to_string()));
        }

        let listen_addresses = if settings.listen_on.is_empty() {
            vec![settings.ip_version_only.wildcard_ip().to_string()]
        } else {
            settings.listen_on.clone()
        };

        // 所有监听 socket 共享同一份会话表：端口跳跃时同一个 conv 会从新端口、
        // 新源地址到达（对应 C++ `kcp_channels` 在所有 `udp_server` 之间共享）。
        let state = Arc::new(SocketState::new(settings.clone(), codec));

        let mut tasks = JoinSet::new();
        for address in listen_addresses {
            let ip: IpAddr = address
                .parse()
                .map_err(|_| ServerError::Startup(format!("无效的 listen_on 地址：{address}")))?;
            if let Some(want_ipv4) = settings.ip_version_only.want_ipv4() {
                if ip.is_ipv4() != want_ipv4 {
                    return Err(ServerError::Startup(format!(
                        "listen_on 地址 {ip} 与 ipv4_only/ipv6_only 配置冲突"
                    )));
                }
            }
            // 与 C++ `udp_server::initialise` 一致：IPv6 监听地址只有
            // `ipv6_only=yes` 时才设置 IPV6_V6ONLY，否则是双栈。
            let v6only = settings.ip_version_only.ipv6_v6only();
            for port in &settings.listen_ports {
                let addr = SocketAddr::new(ip, *port);
                let socket = match UdpEndpoint::bind_with(
                    addr,
                    UdpBindOptions {
                        reuse_address: true,
                        only_v6: addr.is_ipv6().then_some(v6only),
                    },
                )
                .await
                {
                    Ok(socket) => socket,
                    Err(error) => {
                        let message = format!(
                            "{}Listen Address incorrect - {addr}\n",
                            time_string_with_brackets()
                        );
                        let _ = print_message_to_file(&settings.log_messages, &message);
                        return Err(ServerError::Startup(format!("无法监听 {addr}: {error}")));
                    }
                };
                tracing::info!("服务端监听 UDP {addr}");

                let state = state.clone();
                tasks.spawn(async move { state.run_socket(socket).await });
            }
        }

        // KCP 定时驱动与维护任务只启动一份，避免多个监听 socket 重复 update
        let driver = state.clone();
        tasks.spawn(async move { driver.run_driver().await });

        // 任一监听循环异常退出就上报
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error),
                Err(error) => {
                    return Err(ServerError::Startup(format!("监听任务崩溃：{error}")));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use kcptube_protocol::constant::{KCP_CLEANUP_WAITS, KEEPALIVE_TIMEOUT, TIME_GAP_SECONDS};

    #[test]
    fn timeout_constants_match_cpp() {
        assert_eq!(KEEPALIVE_TIMEOUT, 31);
        assert_eq!(TIME_GAP_SECONDS, 255);
        assert_eq!(KCP_CLEANUP_WAITS, 4);
    }
}
