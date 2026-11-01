// SPDX-License-Identifier: GPL-3.0-only

//! 数据帧处理、KCP 驱动与空闲会话维护。

use crate::ServerError;
use crate::state::{DataSession, HandshakeEntry, SessionKind, SocketState};
use kcptube_kcp::KcpError;
use kcptube_protocol::constant::{KCP_CLEANUP_WAITS, TIME_GAP_SECONDS};
use kcptube_protocol::packet::{
    Feature, ProtocolType, create_keep_alive_response_packet, inform_error_packet, unpack_inner,
};
use kcptube_transport::{
    IncomingFrame, KeepAlive, MAX_INNER_MESSAGE, TransportError, UdpEndpoint, now_ms, now_secs,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

impl SocketState {
    pub(crate) async fn handle_data(
        &self,
        socket: &UdpEndpoint,
        listener_addr: SocketAddr,
        conv: u32,
        peer: SocketAddr,
        frame: IncomingFrame<'_>,
    ) -> Result<(), ServerError> {
        let session = {
            let sessions = self.sessions.lock().await;
            sessions.get(&conv).cloned()
        };
        let Some(session) = session else {
            return Ok(());
        };

        // 端口跳跃：同一个 conv 可能从新的服务端端口 / 客户端源地址到达，
        // 立即把回包 socket 与对端重定向到新位置（对应 C++ `ingress_listener`
        // 与 `ingress_source_endpoint` 的更新）。
        let socket_changed = session.channel.local_addr().ok() != Some(listener_addr);
        let peer_changed = session.channel.peer() != peer;
        if socket_changed {
            let old = session.channel.rebind(socket.clone(), peer)?;
            drop(old);
        } else {
            session.channel.set_peer(peer);
        }
        if socket_changed || peer_changed {
            self.log_message(&format!(
                "conv={conv} 端口跳跃：监听 {listener_addr}，对端 {peer}"
            ));
        }
        session.channel.accept_frame(frame).await?;
        tracing::trace!("收到 conv={conv} 的数据帧");
        self.drain_data(&session).await;
        Ok(())
    }

    async fn drain_data(&self, session: &DataSession) {
        // mux 会话的 KCP 收发由 `MuxTunnel::run` 独占，这里不消费它的消息
        if matches!(session.kind, SessionKind::Mux { .. }) {
            return;
        }
        let mut buffer = vec![0u8; MAX_INNER_MESSAGE];
        loop {
            let size = match session.channel.receive(&mut buffer).await {
                Ok(size) => size,
                Err(TransportError::Kcp(KcpError::WouldBlock)) => break,
                Err(_) => {
                    session.begin_close(now_secs() - KCP_CLEANUP_WAITS);
                    break;
                }
            };

            let Ok((feature, protocol, body)) = unpack_inner(&buffer[..size]) else {
                session.begin_close(now_secs() - KCP_CLEANUP_WAITS);
                break;
            };
            match feature {
                Feature::RawData if protocol == ProtocolType::Tcp => {
                    let SessionKind::Tcp(writer) = &session.kind else {
                        continue;
                    };
                    tracing::trace!("转发 {} 字节到本地 TCP", body.len());
                    let mut writer = writer.lock().await;
                    if writer.write_all(body).await.is_err() {
                        session.begin_close(now_secs() - KCP_CLEANUP_WAITS);
                        break;
                    }
                    session.mark_send(now_secs());
                }
                Feature::RawData if protocol == ProtocolType::Udp => {
                    let SessionKind::Udp { socket, target } = &session.kind else {
                        continue;
                    };
                    tracing::trace!("转发 {} 字节到本地 UDP {target}", body.len());
                    if socket.try_send_to(body, *target).is_ok() {
                        session.mark_send(now_secs());
                    }
                }
                Feature::KeepAlive => {
                    let _ = session
                        .channel
                        .send(&create_keep_alive_response_packet(protocol))
                        .await;
                }
                Feature::KeepAliveResponse => {
                    session.channel.mark_keep_alive_response(now_secs());
                }
                Feature::Disconnect | Feature::Failure => {
                    // 对端已主动断开，不再回发 disconnect（对应 C++ `inform_peer = false`）
                    session.begin_close(now_secs() - KCP_CLEANUP_WAITS);
                    break;
                }
                _ => {}
            }
        }
    }

    /// 每 5ms 驱动所有 KCP 会话
    pub(crate) async fn tick_kcp(&self) {
        let now = now_ms();

        let handshakes: Vec<Arc<HandshakeEntry>> =
            self.handshakes.lock().await.values().cloned().collect();
        for entry in handshakes {
            let _ = entry.channel.update(now).await;
        }

        let sessions: Vec<Arc<DataSession>> =
            self.sessions.lock().await.values().cloned().collect();
        for session in sessions {
            // mux 会话的 KCP 由 `MuxTunnel::run` 自己驱动
            if matches!(session.kind, SessionKind::Mux { .. }) {
                continue;
            }
            if session.channel.update(now).await.is_err() {
                continue;
            }
            self.drain_data(&session).await;
        }
    }

    /// 每秒执行一次：回收过期握手 / 关闭空闲会话 / 发送保活
    pub(crate) async fn maintain(&self) {
        let now = now_secs();

        // 1) 握手通道：创建后 `KCP_CLEANUP_WAITS` 秒回收
        //    （C++ 在 4 秒时清空输出回调、8 秒时移出表；这里直接回收）
        {
            let mut handshakes = self.handshakes.lock().await;
            handshakes.retain(|_, entry| now - entry.created_at < KCP_CLEANUP_WAITS);
        }

        // 2) 数据会话
        let sessions: Vec<Arc<DataSession>> =
            self.sessions.lock().await.values().cloned().collect();
        let keep_alive = i64::from(self.settings.keep_alive);

        for session in &sessions {
            if session.is_closed() {
                if now - session.expire_at() >= KCP_CLEANUP_WAITS {
                    let mut map = self.sessions.lock().await;
                    map.retain(|_, item| !std::ptr::eq(item.as_ref(), session.as_ref()));
                    tracing::debug!("回收已关闭的 KCP 会话");
                }
                continue;
            }

            let protocol = session.protocol;

            // UDP：两个方向都超过 `udp_timeout` 才回收（对应 C++ 的
            // `time_gap_of_receive() > udp_timeout && time_gap_of_send() > udp_timeout`）
            if protocol == ProtocolType::Udp {
                let udp_timeout = i64::from(self.settings.udp_timeout);
                let receive_gap = now - session.last_receive_at();
                let send_gap = now - session.last_send_at();
                if receive_gap > udp_timeout && send_gap > udp_timeout {
                    session.begin_close(now - KCP_CLEANUP_WAITS);
                    self.log_message(&format!(
                        "UDP 会话空闲 {receive_gap} 秒（发送 {send_gap} 秒），已关闭"
                    ));
                    continue;
                }
            }

            // 对应 C++ `clean_by_force = kcp_last_activity_gap > gbv_time_gap_seconds`
            let idle = (now - session.channel.last_input_time().await).abs();
            if idle > TIME_GAP_SECONDS {
                if let SessionKind::Mux { tunnel, .. } = &session.kind {
                    tunnel.shutdown();
                    session.begin_close(now - i64::from(self.settings.udp_timeout));
                    self.log_message(&format!("mux 会话空闲 {idle} 秒，已关闭"));
                    continue;
                }
                let packet = inform_error_packet(protocol, "TCP Session Closed");
                let _ = session.channel.send(&packet).await;
                let _ = session.channel.update(now_ms()).await;
                session.begin_close(now - i64::from(self.settings.udp_timeout));
                self.log_message(&format!("KCP 会话空闲 {idle} 秒，已关闭"));
                continue;
            }

            if keep_alive > 0
                && !matches!(session.kind, SessionKind::Mux { .. })
                && now >= session.next_keep_alive()
            {
                // 与 C++ 一致：发送失败也推进下次时刻，避免每个 tick 重试
                if KeepAlive::tick_at(
                    &session.channel,
                    protocol,
                    keep_alive,
                    session.next_keep_alive(),
                    now,
                )
                .await
                .is_ok()
                {
                    let _ = session.channel.update(now_ms()).await;
                }
                session.set_next_keep_alive(now + keep_alive);
            }
        }
    }
}
