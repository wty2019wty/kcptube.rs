// SPDX-License-Identifier: GPL-3.0-only

//! ingress 监听：解密、拆包、按 `conv` 分派到握手或数据会话。
//!
//! 与 C++ `relay_mode::udp_listener_incoming` 对应。区别是这里**不在 listener
//! 循环里驱动 KCP**：解密后的明文帧投递给对应的会话任务，避免慢会话阻塞
//! 同一 socket 上的其它连接。

use crate::RelayError;
use crate::state::RelayState;
use kcptube_transport::{TransportError, UdpEndpoint, is_connection_reset, parse_frame};
use std::sync::Arc;

/// 单个 UDP 监听 socket 的收包循环。
pub(crate) async fn run_listener(
    state: Arc<RelayState>,
    socket: UdpEndpoint,
) -> Result<(), RelayError> {
    let mut buffer = vec![0u8; 65536];
    loop {
        let (size, peer) = match socket.recv_from(&mut buffer).await {
            Ok(received) => received,
            Err(error) if is_connection_reset(&error) => continue,
            Err(error) => {
                tracing::warn!("relay UDP 接收失败：{error}");
                continue;
            }
        };

        let datagram = &buffer[..size];
        state.listener_counters.add_ingress_raw(size);
        let Ok((_, plain)) = state.ingress_codec.open_frame(datagram) else {
            continue;
        };
        state.listener_counters.add_ingress_inner(plain.len());
        let Ok(frame) = parse_frame(&plain, state.ingress_fec_original) else {
            continue;
        };
        let Some(conv) = frame.conv() else {
            continue;
        };

        if conv == 0 {
            if let Err(error) = state.handle_handshake_datagram(&socket, peer, plain).await {
                tracing::debug!("relay 握手包处理失败：{error}");
            }
            continue;
        }

        let Some(session) = state.session(conv).await else {
            continue;
        };
        // 客户端端口跳跃会更换源端口，按最新来源更新回包目标
        session.update_ingress_peer(peer);
        if session.tx.try_send(plain).is_err() {
            tracing::trace!("relay 会话 conv={conv} 队列已满，丢弃报文");
        }
    }
}

/// 把 `TransportError::Kcp(WouldBlock)` 视为「已取空」，其余错误返回。
pub(crate) fn is_drained(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Kcp(kcptube_kcp::KcpError::WouldBlock)
    )
}
