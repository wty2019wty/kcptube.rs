// SPDX-License-Identifier: GPL-3.0-only

//! 客户端端口跳跃（对应 C++ `change_new_port` / `test_before_change`）。
//!
//! 探测与重绑的 IO 细节在 `kcptube_transport::PortHop`；这里只负责
//! 「按 `dport_refresh` 定时、选择目标、写日志、收尾旧 socket」。

use crate::ClientError;
use crate::state::ClientState;
use kcptube_protocol::constant::{PACKET_LENGTH, RECEIVER_CLEANUP_WAITS};
use kcptube_transport::{KcpChannel, PortHop, UdpEndpoint, resolve_endpoints_filtered};
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

    let hop = Arc::new(PortHop::new(
        state.codec.clone(),
        state.fec,
        state.settings.kcp_mtu,
    ));
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
            match hop_port(&state, &hop, &channel).await {
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

/// 选择新目标并探测，成功时重绑主通道并返回旧 socket。
async fn hop_port(
    state: &Arc<ClientState>,
    hop: &PortHop,
    channel: &Arc<KcpChannel>,
) -> Result<Option<UdpEndpoint>, ClientError> {
    let (host, port) = state.pick_destination().await?;
    let candidates =
        resolve_endpoints_filtered(&host, port, state.settings.ip_version_only.want_ipv4()).await?;

    match hop.probe(channel, &candidates).await? {
        Some(old) => {
            state.log_message(&format!("端口跳跃：切换到 {host}:{port}"));
            Ok(Some(old))
        }
        None => Ok(None),
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
