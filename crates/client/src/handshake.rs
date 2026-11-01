// SPDX-License-Identifier: GPL-3.0-only

//! 数据通道握手（对应 C++ `create_handshake` + `handshake_timeout_detection`）。

use crate::ClientError;
use crate::port_hop::spawn_port_hopper;
use crate::state::ClientState;
use crate::util::{data_config, handshake_config, wildcard_addr};
use kcptube_kcp::KcpError;
use kcptube_protocol::constant::{HANDSHAKE_TIMEOUT, PACKET_LENGTH};
use kcptube_protocol::packet::{
    Feature, ProtocolType, SettingsWrapper, request_initialise_packet,
    request_initialise_packet_with_address, unpack_inner,
};
use kcptube_transport::{
    KcpChannel, MAX_INNER_MESSAGE, TransportError, UdpEndpoint, now_ms, resolve_endpoints_filtered,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::time::{Duration, Instant, interval};

/// 建立一条数据通道（含握手与 30 秒超时换端口重试）。
///
/// 与 C++ `create_handshake` + `handshake_timeout_detection` 的行为一致：
/// 超时后丢弃旧 socket，重新解析服务端地址并发起新握手。
///
/// 与 C++ 的差异（更稳健）：域名解析出的**所有**地址会依次尝试。
/// C++ 用双栈 socket + `v4_mapped` 解析，天然覆盖 A/AAAA 两种记录；
/// Rust 的 socket 有明确地址族，因此某个地址不可达时继续尝试下一个，
/// 避免「DDNS 同时有 A/AAAA 记录、服务端只监听 IPv4」时永远连不上。
pub(crate) async fn establish_channel(
    state: &Arc<ClientState>,
    protocol: ProtocolType,
    remote: Option<&(String, u16)>,
) -> Result<(u32, Arc<KcpChannel>), ClientError> {
    loop {
        let (host, port) = state.pick_destination().await?;
        let candidates = match resolve_endpoints_filtered(
            &host,
            port,
            state.settings.ip_version_only.want_ipv4(),
        )
        .await
        {
            Ok(candidates) => candidates,
            Err(error) => {
                state.log_message(&format!("无法解析 {host}:{port}：{error}"));
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        // 多个候选地址共享同一个握手超时预算：只有一个候选时保持 C++ 的
        // 30 秒；有多个时均分（每个至少 5 秒），避免第一个地址不可达时
        // 把整段时间耗在它身上。
        let per_candidate =
            Duration::from_secs((HANDSHAKE_TIMEOUT as u64 / candidates.len() as u64).max(5));

        for server_peer in candidates {
            match try_establish_channel(state, protocol, remote, &host, server_peer, per_candidate)
                .await
            {
                Ok(Some(result)) => return Ok(result),
                // 该候选地址握手失败（超时 / 对端不可达）：换下一个地址
                Ok(None) => continue,
                // 服务端明确返回 failure：本地连接已经没有意义
                Err(error) => return Err(error),
            }
        }
    }
}

/// 对单个已解析的服务端地址尝试一次握手。
///
/// - `Ok(Some(..))`：成功
/// - `Ok(None)`：本次失败（超时、ICMP 不可达等），调用方可换地址/换端口重试
/// - `Err(..)`：服务端明确拒绝（`failure`），不应重试
async fn try_establish_channel(
    state: &Arc<ClientState>,
    protocol: ProtocolType,
    remote: Option<&(String, u16)>,
    host: &str,
    server_peer: SocketAddr,
    timeout: Duration,
) -> Result<Option<(u32, Arc<KcpChannel>)>, ClientError> {
    let socket = match UdpEndpoint::bind(wildcard_addr(server_peer)).await {
        Ok(socket) => socket,
        Err(error) => {
            state.log_message(&format!("无法创建本地 UDP 套接字：{error}"));
            return Ok(None);
        }
    };

    let handshake = Arc::new(
        KcpChannel::new_with_fec(
            state.codec.clone(),
            handshake_config(state.settings.kcp_mtu),
            socket.clone(),
            server_peer,
            state.fec,
        )
        .await?,
    );
    handshake.set_blast(state.settings.blast);
    handshake
        .set_bandwidth(
            state.settings.outbound_bandwidth,
            state.settings.inbound_bandwidth,
        )
        .await;
    {
        let mut kcp = handshake.endpoint.kcp.lock().await;
        kcp.set_rx_minrto(10);
    }

    // 对应 C++ `create_handshake`：仅在 `ignore_listen_address` /
    // `ignore_listen_port` 场景（此时一定来自 `[custom_input*]` 映射）
    // 才把用户指定的目标地址随握手包发给服务端。
    let packet = match remote {
        Some((address, port)) => request_initialise_packet_with_address(
            protocol,
            state.settings.outbound_bandwidth,
            state.settings.inbound_bandwidth,
            address,
            *port,
        ),
        None => request_initialise_packet(
            protocol,
            state.settings.outbound_bandwidth,
            state.settings.inbound_bandwidth,
        ),
    };
    // 发送/驱动失败（例如本机没有对应地址族的路由）只算本次尝试失败，
    // 不能直接中断本地连接；交给上层换地址或换端口重试。
    if let Err(error) = handshake.send(&packet).await {
        tracing::warn!("握手包发送失败（{server_peer}）：{error}");
        return Ok(None);
    }
    if let Err(error) = handshake.update(now_ms()).await {
        tracing::warn!("握手通道驱动失败（{server_peer}）：{error}");
        return Ok(None);
    }

    match wait_handshake(state, &handshake, &socket, timeout).await {
        Ok(response) => {
            // 服务端会把自己的监听端口范围回传，客户端据此更新跳跃端口池
            state
                .update_destination_ports(response.port_start, response.port_end)
                .await;

            let conv = response.uid;
            if conv == 0 {
                state.log_message("服务端返回的 conv 为 0，重新握手");
                return Ok(None);
            }
            tracing::debug!("握手完成，conv={conv}（服务端 {host} -> {server_peer}）");

            let channel = Arc::new(
                KcpChannel::new_with_fec(
                    state.codec.clone(),
                    data_config(&state.settings, conv),
                    socket.clone(),
                    server_peer,
                    state.fec,
                )
                .await?,
            );
            channel.set_blast(state.settings.blast);
            // 对应 C++：客户端出方向带宽受服务端入方向带宽限制
            let outbound = if response.inbound_bandwidth > 0 {
                state
                    .settings
                    .outbound_bandwidth
                    .min(response.inbound_bandwidth)
            } else {
                state.settings.outbound_bandwidth
            };
            channel
                .set_bandwidth(outbound, state.settings.inbound_bandwidth)
                .await;
            channel.reset_window_values(handshake.rx_srtt().await).await;
            {
                let mut kcp = channel.endpoint.kcp.lock().await;
                kcp.set_rx_minrto(10);
                kcp.update(now_ms());
            }

            // 端口跳跃：定时探测新目标并切换发送 socket
            spawn_port_hopper(state, &channel);
            Ok(Some((conv, channel)))
        }
        Err(ClientError::HandshakeTimeout) => {
            state.log_message(&format!(
                "握手超时 {} 秒，切换新的本地端口重试",
                timeout.as_secs()
            ));
            Ok(None)
        }
        Err(ClientError::Startup(message)) => {
            state.log_message(&message);
            Err(ClientError::Startup(message))
        }
        Err(error) => {
            tracing::warn!("握手失败（{server_peer}）：{error}");
            Ok(None)
        }
    }
}

async fn wait_handshake(
    state: &Arc<ClientState>,
    channel: &Arc<KcpChannel>,
    socket: &UdpEndpoint,
    timeout: Duration,
) -> Result<SettingsWrapper, ClientError> {
    let deadline = Instant::now() + timeout;
    let mut ticker = interval(Duration::from_millis(5));
    let mut datagram = vec![0u8; PACKET_LENGTH + 256];
    let mut inner_buffer = vec![0u8; MAX_INNER_MESSAGE];

    loop {
        if Instant::now() >= deadline {
            return Err(ClientError::HandshakeTimeout);
        }

        tokio::select! {
            _ = ticker.tick() => {
                channel.update(now_ms()).await?;
            }
            result = socket.recv_from(&mut datagram) => {
                // 对应 C++ `udp_client::handle_receive`：底层 recv 出错（例如
                // 对端暂时不在线导致的 ICMP 端口不可达 WSAECONNRESET）只是
                // 重新挂起接收，**不会中断握手**；由 30 秒超时统一兜底。
                let Ok((size, _)) = result else {
                    tracing::trace!("握手期间 UDP 接收失败，继续重试");
                    continue;
                };
                state.counters.add_ingress_raw(size);
                if channel.process_datagram(&datagram[..size]).await.is_err() {
                    continue;
                }
                loop {
                    let size = match channel.receive(&mut inner_buffer).await {
                        Ok(size) => size,
                        Err(TransportError::Kcp(KcpError::WouldBlock)) => break,
                        Err(error) => return Err(error.into()),
                    };
                    let (feature, _protocol, body) = unpack_inner(&inner_buffer[..size])?;
                    match feature {
                        Feature::Initialise => {
                            let settings = SettingsWrapper::decode(body)?;
                            if settings.uid != 0 {
                                return Ok(settings);
                            }
                        }
                        Feature::Failure => {
                            return Err(ClientError::Startup(format!(
                                "failure, error message: {}",
                                String::from_utf8_lossy(body)
                            )));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
