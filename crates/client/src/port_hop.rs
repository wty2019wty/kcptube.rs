// SPDX-License-Identifier: GPL-3.0-only

//! 端口跳跃（对应 C++ `change_new_port` / `test_before_change`）。

use crate::ClientError;
use crate::state::ClientState;
use kcptube_kcp::KcpConfig;
use kcptube_kcp::KcpError;
use kcptube_protocol::constant::{PACKET_LENGTH, RECEIVER_CLEANUP_WAITS};
use kcptube_protocol::packet::{Feature, create_test_connection_packet, unpack_inner};
use kcptube_transport::{
    KcpChannel, MAX_INNER_MESSAGE, TransportError, UdpEndpoint, now_ms, resolve_endpoints_filtered,
    wildcard_addr,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::time::{Duration, Instant, interval};

/// 定时端口跳跃任务（对应 C++ `change_new_port` / `test_before_change`）。
///
/// 每 `dport_refresh` 秒通过一条临时握手通道发送 `test_connection`，
/// 收到响应后用新的目标地址与本地 socket 重绑主通道。
pub(crate) fn spawn_port_hopper(state: &Arc<ClientState>, channel: &Arc<KcpChannel>) {
    let refresh = state.settings.dynamic_port_refresh;
    if refresh <= 0 {
        return;
    }

    let weak = Arc::downgrade(channel);
    let state = state.clone();
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(refresh as u64));
        // `interval` 的第一次 tick 立即完成，先跳过
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let Some(channel) = weak.upgrade() else {
                break;
            };
            match hop_port(&state, &channel).await {
                Ok(Some(old_socket)) => {
                    // 旧 socket 继续收 4 秒，尽量接住切换瞬间在途的报文
                    // （对应 C++ `expiring_forwarders` 在 4 秒后 stop）
                    let reader = channel.clone();
                    tokio::spawn(async move {
                        drain_old_socket(reader, old_socket).await;
                    });
                }
                Ok(None) => {}
                Err(error) => tracing::warn!("端口跳跃失败：{error}"),
            }
        }
    });
}

/// 探测新端口，成功返回旧 socket。
async fn hop_port(
    state: &Arc<ClientState>,
    channel: &Arc<KcpChannel>,
) -> Result<Option<UdpEndpoint>, ClientError> {
    let (host, port) = state.pick_destination().await?;
    let candidates =
        resolve_endpoints_filtered(&host, port, state.settings.ip_version_only.want_ipv4()).await?;

    for peer in candidates {
        if let Some(old) = probe_candidate(state, channel, &host, port, peer).await? {
            return Ok(Some(old));
        }
    }
    Ok(None)
}

/// 用一条临时握手通道向指定地址发送 `test_connection`，成功后重绑主通道。
async fn probe_candidate(
    state: &Arc<ClientState>,
    channel: &Arc<KcpChannel>,
    host: &str,
    port: u16,
    peer: SocketAddr,
) -> Result<Option<UdpEndpoint>, ClientError> {
    let socket = match UdpEndpoint::bind(wildcard_addr(peer)).await {
        Ok(socket) => socket,
        Err(error) => {
            tracing::trace!("端口跳跃无法绑定本地 socket（{peer}）：{error}");
            return Ok(None);
        }
    };

    let probe = Arc::new(
        KcpChannel::new_with_fec(
            state.codec.clone(),
            KcpConfig::handshake(state.settings.kcp_mtu),
            socket.clone(),
            peer,
            state.fec,
        )
        .await?,
    );
    // 探测通道只需要 blast 与最小重传超时（与 C++ `test_before_change` 一致地
    // 不参与带宽限制），不写入数据通道的窗口参数。
    probe.set_blast(true);
    probe.set_rx_minrto(10).await;
    probe.send(&create_test_connection_packet()).await?;
    probe.update(now_ms()).await?;

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut ticker = interval(Duration::from_millis(5));
    let mut datagram = vec![0u8; PACKET_LENGTH + 256];
    let mut inner = vec![0u8; MAX_INNER_MESSAGE];

    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::select! {
            _ = ticker.tick() => {
                probe.update(now_ms()).await?;
            }
            result = socket.recv_from(&mut datagram) => {
                if let Ok((size, _)) = result {
                    let _ = probe.process_datagram(&datagram[..size]).await;
                }
            }
            _ = tokio::time::sleep_until(deadline) => return Ok(None),
        }

        loop {
            match probe.receive(&mut inner).await {
                Ok(size) => {
                    let (feature, _, _) = unpack_inner(&inner[..size])?;
                    // `test_connection` 与 `keep_alive` 共用同一 feature 值
                    if matches!(feature, Feature::KeepAlive | Feature::KeepAliveResponse) {
                        let old = channel.rebind(socket.clone(), peer)?;
                        state.log_message(&format!("端口跳跃：切换到 {host}:{port}"));
                        return Ok(Some(old));
                    }
                }
                Err(TransportError::Kcp(KcpError::WouldBlock)) => break,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

/// 切换后继续读取旧 socket 一段时间，避免在途 ACK 被直接丢弃。
async fn drain_old_socket(channel: Arc<KcpChannel>, socket: UdpEndpoint) {
    let deadline = Instant::now() + Duration::from_secs((RECEIVER_CLEANUP_WAITS / 2).max(1) as u64);
    let mut buffer = vec![0u8; PACKET_LENGTH + 256];
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            result = socket.recv_from(&mut buffer) => {
                match result {
                    Ok((size, _)) => {
                        let _ = channel.process_datagram(&buffer[..size]).await;
                    }
                    Err(_) => break,
                }
            }
        }
    }
}
