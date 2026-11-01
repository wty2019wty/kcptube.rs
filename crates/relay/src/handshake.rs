// SPDX-License-Identifier: GPL-3.0-only

//! 握手改写与数据会话建立（对应 C++ `udp_listener_incoming_new_connection`
//! 与 `create_kcp_bidirections`）。
//!
//! 握手在两条 conv 0 的 KCP 通道之间中继：
//!
//! ```text
//! client --initialise--> [relay ingress] --改写后--> [relay egress] --> server
//! client <--initialise(uid)--          <--改写后--
//! 之后两侧各建一条 conv=uid 的数据通道，帧原样互转
//! ```
//!
//! 改写只覆盖 `settings_wrapper` 的 26 字节头（带宽与端口范围），
//! 负载长度不变，因此不影响 KCP 消息边界。

use crate::RelayError;
use crate::listener::is_drained;
use crate::state::{HandshakeEntry, RelaySession, RelayState};
use crate::util::{is_continuous, random_pick};
use kcptube_kcp::KcpConfig;
use kcptube_protocol::constant::{HANDSHAKE_TIMEOUT, KCP_CLEANUP_WAITS, PACKET_LENGTH};
use kcptube_protocol::packet::{
    Feature, SettingsWrapper, create_keep_alive_response_packet, create_test_connection_packet,
    unpack_inner,
};
use kcptube_transport::{
    KcpChannel, MAX_INNER_MESSAGE, UdpEndpoint, now_ms, now_secs, open_handshake_channel,
    parse_frame, resolve_endpoints_filtered,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, interval};

impl RelayState {
    /// listener 收到的 conv 0 明文帧：复用已有的握手条目，或新建后启动入口任务。
    pub(crate) async fn handle_handshake_datagram(
        self: &Arc<Self>,
        socket: &UdpEndpoint,
        peer: SocketAddr,
        plain: Vec<u8>,
    ) -> Result<(), RelayError> {
        if let Some(entry) = self.handshakes.lock().await.get(&peer).cloned() {
            let _ = entry.tx.try_send(plain);
            return Ok(());
        }

        let (tx, rx) = mpsc::channel(64);
        let channel = Arc::new(
            KcpChannel::new_with_fec(
                self.ingress_codec.clone(),
                KcpConfig::handshake(self.ingress.kcp_mtu),
                socket.clone(),
                peer,
                self.ingress_fec,
            )
            .await?,
        );
        channel
            .apply_handshake_tuning(
                self.ingress.blast,
                self.ingress.outbound_bandwidth,
                self.ingress.inbound_bandwidth,
            )
            .await;

        let entry = Arc::new(HandshakeEntry::new(peer, channel, tx));
        self.handshakes.lock().await.insert(peer, entry.clone());
        let _ = entry.tx.try_send(plain);

        let state = self.clone();
        tokio::spawn(async move {
            run_handshake_entry(state, entry, rx).await;
        });
        Ok(())
    }

    /// 选择 egress 的目标地址与端口（对应 C++ `randomly_pick_index`）。
    fn pick_egress_target(&self) -> Result<(String, u16), RelayError> {
        let host = random_pick(&self.egress.destination_address_list)
            .cloned()
            .ok_or_else(|| RelayError::Startup("destination_address is not set".to_string()))?;
        let port = random_pick(&self.egress.destination_ports)
            .copied()
            .ok_or_else(|| RelayError::Startup("destination_port is not set".to_string()))?;
        Ok((host, port))
    }
}

/// 握手条目任务：驱动 ingress 侧 KCP，直到条目过期或会话建立。
async fn run_handshake_entry(
    state: Arc<RelayState>,
    entry: Arc<HandshakeEntry>,
    mut rx: mpsc::Receiver<Vec<u8>>,
) {
    let mut ticker = interval(Duration::from_millis(5));
    let mut inner = vec![0u8; MAX_INNER_MESSAGE];
    loop {
        tokio::select! {
            received = rx.recv() => {
                let Some(plain) = received else { break };
                if let Err(error) = state
                    .process_ingress_handshake(&entry, &plain, &mut inner)
                    .await
                {
                    tracing::debug!("relay 握手处理失败：{error}");
                }
            }
            _ = ticker.tick() => {
                let _ = entry.ingress.update(now_ms()).await;
                if let Some(egress) = entry.egress.lock().await.clone() {
                    let _ = egress.update(now_ms()).await;
                }
                // 与 C++ 一致：握手条目保留 `KCP_CLEANUP_WAITS * 2` 秒
                if now_secs() - entry.created_at > KCP_CLEANUP_WAITS * 2 {
                    break;
                }
            }
        }
    }
    state.handshakes.lock().await.remove(&entry.peer);
}

/// 处理 ingress 侧握手通道上的一条完整内层帧。
impl RelayState {
    async fn process_ingress_handshake(
        self: &Arc<Self>,
        entry: &Arc<HandshakeEntry>,
        plain: &[u8],
        inner: &mut [u8],
    ) -> Result<(), RelayError> {
        let frame = parse_frame(plain, self.ingress_fec_original)?;
        entry.ingress.accept_frame(frame).await?;

        loop {
            let size = match entry.ingress.receive(inner).await {
                Ok(size) => size,
                Err(error) if is_drained(&error) => break,
                Err(error) => return Err(error.into()),
            };
            let (feature, _protocol, _) = unpack_inner(&inner[..size])?;
            match feature {
                Feature::Initialise => {
                    self.forward_initialise(entry, &inner[..size], size).await?;
                }
                Feature::KeepAlive => {
                    if !entry.is_established() && entry.egress.lock().await.is_none() {
                        // 新条目上的 `test_connection`：由 relay 本地回，不打扰服务端
                        entry.ingress.send(&create_test_connection_packet()).await?;
                        entry.ingress.update(now_ms()).await?;
                    } else if let Some(egress) = entry.egress.lock().await.clone() {
                        egress.send(&inner[..size]).await?;
                    }
                }
                Feature::KeepAliveResponse => {
                    entry.ingress.mark_keep_alive_response(now_secs());
                }
                Feature::Failure | Feature::Disconnect => {
                    if let Some(egress) = entry.egress.lock().await.clone() {
                        egress.send(&inner[..size]).await?;
                    }
                    break;
                }
                _ => {
                    if let Some(egress) = entry.egress.lock().await.clone() {
                        egress.send(&inner[..size]).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// 改写客户端 `initialise` 并转发给服务端，必要时先建立 egress 握手通道。
    async fn forward_initialise(
        self: &Arc<Self>,
        entry: &Arc<HandshakeEntry>,
        payload: &[u8],
        size: usize,
    ) -> Result<(), RelayError> {
        let (_, _, body) = unpack_inner(&payload[..size])?;
        let mut settings = SettingsWrapper::decode(body)?;

        // 对应 C++ `relay.cpp:443`–`447`：与 egress 的上限取小；
        // 注意 egress 未限制（0）时也会把客户端的限制清零，这是上游行为。
        settings.inbound_bandwidth =
            clamp_bandwidth(settings.inbound_bandwidth, self.egress.inbound_bandwidth);
        settings.outbound_bandwidth =
            clamp_bandwidth(settings.outbound_bandwidth, self.egress.outbound_bandwidth);

        self.ensure_egress_handshake(entry).await?;
        let Some(egress) = entry.egress.lock().await.clone() else {
            return Err(RelayError::Startup("egress 握手通道尚未建立".to_string()));
        };

        // 只改写 26 字节头，保持负载长度（data_layer 头 1 字节）
        let offset = size - body.len();
        let mut packet = payload[..size].to_vec();
        settings.modify_details(&mut packet[offset..])?;

        egress.send(&packet).await?;
        egress.update(now_ms()).await?;
        Ok(())
    }

    /// 建立面向服务端的独占 socket 握手通道（若尚未建立）。
    async fn ensure_egress_handshake(
        self: &Arc<Self>,
        entry: &Arc<HandshakeEntry>,
    ) -> Result<(), RelayError> {
        if entry.egress.lock().await.is_some() {
            return Ok(());
        }

        let (host, port) = self.pick_egress_target()?;
        let candidates =
            resolve_endpoints_filtered(&host, port, self.egress.ip_version_only.want_ipv4())
                .await?;

        for peer in candidates {
            let (channel, socket) = match open_handshake_channel(
                self.egress_codec.clone(),
                self.egress.kcp_mtu,
                peer,
                self.egress_fec,
            )
            .await
            {
                Ok(pair) => pair,
                Err(error) => {
                    tracing::trace!("relay egress 握手通道创建失败（{peer}）：{error}");
                    continue;
                }
            };
            channel
                .apply_handshake_tuning(
                    self.egress.blast,
                    self.egress.outbound_bandwidth,
                    self.egress.inbound_bandwidth,
                )
                .await;

            let channel = Arc::new(channel);
            *entry.egress.lock().await = Some(channel);
            *entry.egress_socket.lock().await = Some(socket.clone());
            *entry.target.lock().await = Some(peer);

            let state = self.clone();
            let entry = entry.clone();
            tokio::spawn(async move {
                run_egress_handshake(state, entry, socket).await;
            });
            return Ok(());
        }

        self.log_message(&format!("无法连接 relay 目标 {host}:{port}"));
        Err(RelayError::Startup(format!(
            "无法连接 relay 目标 {host}:{port}"
        )))
    }
}

/// egress 握手通道任务：接收服务端响应，建立数据会话后退出。
async fn run_egress_handshake(
    state: Arc<RelayState>,
    entry: Arc<HandshakeEntry>,
    socket: UdpEndpoint,
) {
    let Some(egress) = entry.egress.lock().await.clone() else {
        return;
    };
    let mut buffer = vec![0u8; PACKET_LENGTH + 256];
    let mut inner = vec![0u8; MAX_INNER_MESSAGE];
    let deadline = Instant::now() + Duration::from_secs(HANDSHAKE_TIMEOUT as u64);

    loop {
        if Instant::now() >= deadline {
            break;
        }
        tokio::select! {
            result = socket.recv_from(&mut buffer) => {
                let Ok((size, _)) = result else { continue };
                let Ok(()) = egress.process_datagram(&buffer[..size]).await else { continue };
                if let Err(error) = state
                    .drain_egress_handshake(&entry, &egress, &mut inner)
                    .await
                {
                    tracing::debug!("relay egress 握手处理失败：{error}");
                }
                if entry.is_established() {
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

/// 处理 egress 侧握手通道上的一条完整内层帧。
impl RelayState {
    async fn drain_egress_handshake(
        self: &Arc<Self>,
        entry: &Arc<HandshakeEntry>,
        egress: &Arc<KcpChannel>,
        inner: &mut [u8],
    ) -> Result<(), RelayError> {
        loop {
            let size = match egress.receive(inner).await {
                Ok(size) => size,
                Err(error) if is_drained(&error) => break,
                Err(error) => return Err(error.into()),
            };
            let (feature, protocol, body) = unpack_inner(&inner[..size])?;
            match feature {
                Feature::Initialise => {
                    let mut settings = SettingsWrapper::decode(body)?;
                    if settings.uid == 0 {
                        continue;
                    }

                    // 端口范围替换成 relay 自己的（ingress 端口连续时）
                    if is_continuous(&self.ingress.listen_ports) {
                        settings.port_start = *self.ingress.listen_ports.first().unwrap_or(&0);
                        settings.port_end = *self.ingress.listen_ports.last().unwrap_or(&0);
                    }
                    settings.inbound_bandwidth =
                        clamp_bandwidth(settings.inbound_bandwidth, self.ingress.inbound_bandwidth);
                    settings.outbound_bandwidth = clamp_bandwidth(
                        settings.outbound_bandwidth,
                        self.ingress.outbound_bandwidth,
                    );

                    let offset = size - body.len();
                    let mut packet = inner[..size].to_vec();
                    settings.modify_details(&mut packet[offset..])?;
                    entry.ingress.send(&packet).await?;
                    entry.ingress.update(now_ms()).await?;

                    self.create_session(entry.clone(), settings.uid).await?;
                    entry.established.store(true, Ordering::SeqCst);
                }
                Feature::KeepAlive => {
                    egress
                        .send(&create_keep_alive_response_packet(protocol))
                        .await?;
                }
                Feature::KeepAliveResponse => {
                    egress.mark_keep_alive_response(now_secs());
                }
                Feature::Failure | Feature::Disconnect => {
                    entry.ingress.send(&inner[..size]).await?;
                    break;
                }
                _ => {
                    entry.ingress.send(&inner[..size]).await?;
                }
            }
        }
        Ok(())
    }

    /// 建立 conv=uid 的双向数据通道，并启动会话任务。
    async fn create_session(
        self: &Arc<Self>,
        entry: Arc<HandshakeEntry>,
        conv: u32,
    ) -> Result<(), RelayError> {
        if self.session(conv).await.is_some() {
            return Ok(());
        }

        let Some(egress_socket) = entry.egress_socket.lock().await.clone() else {
            return Err(RelayError::Startup("egress socket 不存在".to_string()));
        };
        let Some(egress_peer) = *entry.target.lock().await else {
            return Err(RelayError::Startup("egress 目标不存在".to_string()));
        };
        let listener_socket = entry.ingress.socket();
        let ingress_srtt = entry.ingress.rx_srtt().await;
        let egress_srtt = match entry.egress.lock().await.clone() {
            Some(channel) => channel.rx_srtt().await,
            None => 0,
        };

        let ingress = Arc::new(
            KcpChannel::new_with_fec(
                self.ingress_codec.clone(),
                self.ingress.kcp_config(conv),
                listener_socket,
                entry.peer,
                self.ingress_fec,
            )
            .await?,
        );
        ingress
            .apply_data_tuning(
                self.ingress.blast,
                self.ingress.outbound_bandwidth,
                self.ingress.inbound_bandwidth,
                ingress_srtt,
            )
            .await;

        let egress = Arc::new(
            KcpChannel::new_with_fec(
                self.egress_codec.clone(),
                self.egress.kcp_config(conv),
                egress_socket.clone(),
                egress_peer,
                self.egress_fec,
            )
            .await?,
        );
        egress
            .apply_data_tuning(
                self.egress.blast,
                self.egress.outbound_bandwidth,
                self.egress.inbound_bandwidth,
                egress_srtt,
            )
            .await;

        let (tx, rx) = mpsc::channel(1024);
        let session = Arc::new(RelaySession::new(conv, ingress, egress, egress_socket, tx));
        self.sessions.lock().await.insert(conv, session.clone());

        let state = self.clone();
        tokio::spawn(async move {
            crate::session::run_session(state, session, rx).await;
        });
        Ok(())
    }
}

/// 对应 C++ `if (requested > 0 && requested > limit) requested = limit`。
fn clamp_bandwidth(requested: u64, limit: u64) -> u64 {
    if requested > 0 && requested > limit {
        limit
    } else {
        requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bandwidth_is_clamped_like_cpp() {
        assert_eq!(clamp_bandwidth(1000, 500), 500);
        assert_eq!(clamp_bandwidth(100, 500), 100);
        // 未限制（0）时，客户端申请的带宽会被清零——与 C++ 行为一致
        assert_eq!(clamp_bandwidth(1000, 0), 0);
        assert_eq!(clamp_bandwidth(0, 500), 0);
    }
}
