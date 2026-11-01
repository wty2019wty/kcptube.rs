// SPDX-License-Identifier: GPL-3.0-only

//! 客户端模式（对应 C++ `modes/client.cpp`）。
//!
//! 已实现：TCP + UDP 监听、`initialise` 握手（超时后换新端口重试）、
//! `raw_data` 与 mux 转发、FEC、端口跳跃、保活与状态日志、
//! `ignore_listen_*` + `[custom_input*]` 自定义输入映射。
//! STUN/DDNS 属于 M6。
//!
//! 模块划分：
//! - [`listen`]：监听项计算（常规监听与 `[custom_input*]` 映射）
//! - [`state`]：运行期共享状态
//! - [`handshake`]：数据通道握手
//! - [`tcp`] / [`udp`]：非 mux 模式的本地转发
//! - [`mux`]：mux 模式下的共享隧道池
//! - [`port_hop`]：端口跳跃
//! - [`util`]：随机选择、地址与 KCP 配置等小工具

mod handshake;
mod listen;
mod mux;
mod port_hop;
mod state;
mod tcp;
mod udp;
mod util;

use crate::listen::build_listen_specs;
use crate::mux::{maintain_mux_tunnels, mux_accept_loop, mux_udp_listen_loop};
use crate::state::ClientState;
use crate::tcp::accept_loop;
use crate::udp::udp_listen_loop;
use kcptube_config::UserSettings;
use kcptube_kcp::KcpError;
use kcptube_logging::{print_message_to_file, time_string_with_brackets};
use kcptube_protocol::ProtocolError;
use kcptube_transport::{
    PacketCodec, TcpListenerSession, TransportError, UdpBindOptions, UdpEndpoint,
};
use std::sync::Arc;
use thiserror::Error;
use tokio::task::JoinSet;
use tokio::time::{Duration, interval};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("客户端启动失败：{0}")]
    Startup(String),
    #[error("传输层错误：{0}")]
    Transport(#[from] TransportError),
    #[error("协议错误：{0}")]
    Protocol(#[from] ProtocolError),
    #[error("KCP 错误：{0}")]
    Kcp(#[from] KcpError),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("握手超时")]
    HandshakeTimeout,
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
        let settings = Arc::new(self.settings.clone());
        let codec = Arc::new(PacketCodec::new(
            settings.encryption,
            &settings.encryption_password,
        ));

        if settings.listen_ports.is_empty()
            && !settings.ignore_listen_address
            && !settings.ignore_listen_port
        {
            return Err(ClientError::Startup("listen_port is not set".to_string()));
        }
        if settings.destination_address_list.is_empty() {
            return Err(ClientError::Startup(
                "destination_address is not set".to_string(),
            ));
        }
        if settings.destination_ports.is_empty() {
            return Err(ClientError::Startup(
                "destination_port is not set".to_string(),
            ));
        }

        // `ignore_listen_address` / `ignore_listen_port` 为真时不再监听
        // `listen_on` + `listen_port`，改为逐条监听 `[custom_input*]` 映射
        // （对应 C++ `multiple_listening_tcp` / `multiple_listening_udp`）。
        let listen_specs = build_listen_specs(&settings)?;
        if listen_specs.is_empty() {
            return Err(ClientError::Startup("没有可用的监听地址".to_string()));
        }

        let state = Arc::new(ClientState::new(settings.clone(), codec));

        let mux_mode = settings.mux_tunnels > 0;

        let mut tasks = JoinSet::new();

        // mux 模式：在后台维持 `mux_tunnels` 条共享通道（对应 C++ `establish_mux_channels`）。
        // 建链失败不会让客户端退出，而是持续重试；通道断开后也由该任务补建。
        if mux_mode {
            let mux_state = state.clone();
            let target = settings.mux_tunnels as usize;
            tasks.spawn(async move {
                maintain_mux_tunnels(mux_state, target).await;
                Ok::<(), ClientError>(())
            });
        }

        // 状态摘要任务：只有配置了 `log_status`（即设置了 `log_path`）时才启动，
        // 与 C++ `if (!current_settings.log_status.empty())` 一致
        if state.settings.has_status_log() {
            let state = state.clone();
            tasks.spawn(async move {
                let mut ticker = interval(Duration::from_secs(1));
                loop {
                    ticker.tick().await;
                    state.maybe_log_status().await;
                }
            });
        }

        // 与 C++ `udp_server::initialise` 一致：IPv6 监听地址只有
        // `ipv6_only=yes` 时才设置 IPV6_V6ONLY，否则是双栈。
        let v6only = settings.ip_version_only.ipv6_v6only();
        for spec in listen_specs {
            let addr = spec.address;

            if spec.tcp {
                let listener =
                    match TcpListenerSession::bind_with(addr, addr.is_ipv6().then_some(v6only))
                        .await
                    {
                        Ok(listener) => listener,
                        Err(error) => {
                            let line = format!(
                                "{}Listen Address incorrect - {addr}\n",
                                time_string_with_brackets()
                            );
                            let _ = print_message_to_file(&settings.log_messages, &line);
                            return Err(ClientError::Startup(format!(
                                "无法监听 TCP {addr}: {error}"
                            )));
                        }
                    };
                tracing::info!("客户端监听 TCP {addr}");

                let tcp_state = state.clone();
                let remote = spec.remote.clone();
                if mux_mode {
                    tasks.spawn(async move { mux_accept_loop(listener, tcp_state, remote).await });
                } else {
                    tasks.spawn(async move { accept_loop(listener, tcp_state, remote).await });
                }
            }

            if spec.udp {
                let udp_listener = match UdpEndpoint::bind_with(
                    addr,
                    UdpBindOptions {
                        reuse_address: true,
                        only_v6: addr.is_ipv6().then_some(v6only),
                    },
                )
                .await
                {
                    Ok(endpoint) => endpoint,
                    Err(error) => {
                        let line = format!(
                            "{}Listen Address incorrect - {addr}\n",
                            time_string_with_brackets()
                        );
                        let _ = print_message_to_file(&settings.log_messages, &line);
                        return Err(ClientError::Startup(format!(
                            "无法监听 UDP {addr}: {error}"
                        )));
                    }
                };
                tracing::info!("客户端监听 UDP {addr}");

                let udp_state = state.clone();
                let remote = spec.remote.clone();
                if mux_mode {
                    tasks.spawn(async move {
                        mux_udp_listen_loop(udp_listener, udp_state, remote).await
                    });
                } else {
                    tasks.spawn(
                        async move { udp_listen_loop(udp_listener, udp_state, remote).await },
                    );
                }
            }
        }

        if tasks.is_empty() {
            return Err(ClientError::Startup("没有可用的监听地址".to_string()));
        }

        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error),
                Err(error) => {
                    return Err(ClientError::Startup(format!("监听任务崩溃：{error}")));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use kcptube_protocol::constant::HANDSHAKE_TIMEOUT;

    #[test]
    fn handshake_timeout_matches_cpp() {
        assert_eq!(HANDSHAKE_TIMEOUT, 30);
    }
}
