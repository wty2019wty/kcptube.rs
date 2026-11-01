// SPDX-License-Identifier: GPL-3.0-only

//! 非 mux 模式下的本地 UDP 监听与转发。

use crate::ClientError;
use crate::handshake::establish_channel;
use crate::state::ClientState;
use kcptube_kcp::KcpError;
use kcptube_protocol::constant::{KCP_CLEANUP_WAITS, PACKET_LENGTH};
use kcptube_protocol::packet::{
    Feature, ProtocolType, create_data_packet, create_keep_alive_response_packet,
    inform_disconnect_packet, unpack_inner,
};
use kcptube_transport::{
    KcpChannel, KeepAlive, MAX_INNER_MESSAGE, TransportError, UdpEndpoint, is_connection_reset,
    now_ms, now_secs,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, interval};

/// 本地 UDP 监听：按源地址把数据分发给各自的中继任务。
///
/// 对应 C++ `client_mode::udp_listener_incoming`：每个本地源地址
/// （`udp_local_session_map_to_kcp` 的键）对应一条独立的 KCP 通道。
pub(crate) async fn udp_listen_loop(
    listener: UdpEndpoint,
    state: Arc<ClientState>,
    remote: Option<Arc<(String, u16)>>,
) -> Result<(), ClientError> {
    let mut buffer = vec![0u8; 65536];
    loop {
        let (size, peer) = match listener.recv_from(&mut buffer).await {
            Ok((size, peer)) => (size, peer),
            Err(error) if is_connection_reset(&error) => continue,
            Err(error) => {
                tracing::warn!("UDP 监听接收失败：{error}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };

        state.counters.add_ingress_raw(size);
        let data = buffer[..size].to_vec();

        let existing = state.udp_relays.lock().await.get(&peer).cloned();
        if let Some(sender) = existing {
            // UDP 语义：队列满时丢弃，不阻塞其它本地会话
            match sender.try_send(data.clone()) {
                Ok(()) => continue,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::trace!("UDP 中继队列已满，丢弃来自 {peer} 的数据报");
                    continue;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    state.udp_relays.lock().await.remove(&peer);
                }
            }
        }

        let (sender, receiver) = mpsc::channel(1024);
        let _ = sender.try_send(data);
        state.udp_relays.lock().await.insert(peer, sender);

        let state = state.clone();
        let listener = listener.clone();
        let remote = remote.clone();
        tokio::spawn(async move {
            if let Err(error) =
                udp_relay_task(state.clone(), listener, peer, receiver, remote).await
            {
                tracing::warn!("UDP 中继 {peer} 结束：{error}");
                state.udp_relays.lock().await.remove(&peer);
            }
        });
    }
}

/// 一条本地 UDP 源地址对应一条 KCP 通道。
async fn udp_relay_task(
    state: Arc<ClientState>,
    listener: UdpEndpoint,
    local_peer: SocketAddr,
    from_local: mpsc::Receiver<Vec<u8>>,
    remote: Option<Arc<(String, u16)>>,
) -> Result<(), ClientError> {
    let (conv, channel) = establish_channel(&state, ProtocolType::Udp, remote.as_deref()).await?;
    tracing::debug!("UDP 中继 {local_peer} 握手完成，conv={conv}");

    let id = state.register_channel(conv, channel.clone()).await;
    let result = udp_relay_loop(&state, listener, local_peer, channel, from_local).await;
    state.unregister_channel(id).await;
    result
}

async fn udp_relay_loop(
    state: &Arc<ClientState>,
    listener: UdpEndpoint,
    local_peer: SocketAddr,
    channel: Arc<KcpChannel>,
    mut from_local: mpsc::Receiver<Vec<u8>>,
) -> Result<(), ClientError> {
    let mut ticker = interval(Duration::from_millis(5));
    let mut datagram = vec![0u8; PACKET_LENGTH + 256];
    let mut inner_buffer = vec![0u8; MAX_INNER_MESSAGE];

    let mut keep_alive = KeepAlive::new(i64::from(state.settings.keep_alive), now_secs());
    let udp_timeout = i64::from(state.settings.udp_timeout);
    let mut last_local = now_secs();
    let mut last_remote = now_secs();
    let mut remote_closed = false;

    loop {
        let socket = channel.socket();
        tokio::select! {
            received = from_local.recv() => {
                match received {
                    Some(data) => {
                        last_local = now_secs();
                        let packet = create_data_packet(ProtocolType::Udp, &data);
                        state.counters.add_egress_inner(data.len());
                        channel.send(&packet).await?;
                    }
                    None => {
                        // 监听端已丢弃该映射
                        break;
                    }
                }
            }
            result = socket.recv_from(&mut datagram) => {
                if let Ok((size, _)) = result {
                    state.counters.add_ingress_raw(size);
                    let _ = channel.process_datagram(&datagram[..size]).await;
                }
            }
            _ = ticker.tick() => {
                let now = now_secs();
                keep_alive.tick(&channel, ProtocolType::Udp, now).await?;
                channel.update(now_ms()).await?;

                if now - last_local > udp_timeout && now - last_remote > udp_timeout {
                    tracing::debug!(
                        "UDP 中继 {local_peer} 空闲超过 {udp_timeout} 秒，结束"
                    );
                    break;
                }
            }
        }

        loop {
            let size = match channel.receive(&mut inner_buffer).await {
                Ok(size) => size,
                Err(TransportError::Kcp(KcpError::WouldBlock)) => break,
                Err(error) => return Err(error.into()),
            };
            let (feature, protocol, body) = unpack_inner(&inner_buffer[..size])?;
            match feature {
                Feature::RawData if protocol == ProtocolType::Udp => {
                    last_remote = now_secs();
                    if listener.try_send_to(body, local_peer).is_err() {
                        tracing::trace!("回送 UDP 数据到 {local_peer} 失败");
                    }
                }
                Feature::KeepAlive => {
                    channel
                        .send(&create_keep_alive_response_packet(protocol))
                        .await?;
                }
                Feature::KeepAliveResponse => {
                    channel.mark_keep_alive_response(now_secs());
                }
                Feature::Failure => {
                    state.log_message(&format!(
                        "failure, error message: {}",
                        String::from_utf8_lossy(body)
                    ));
                    remote_closed = true;
                    break;
                }
                Feature::Disconnect => {
                    remote_closed = true;
                    break;
                }
                _ => {}
            }
        }

        if remote_closed {
            break;
        }
    }

    if !remote_closed {
        let _ = channel
            .send(&inform_disconnect_packet(ProtocolType::Udp))
            .await;
        // 保留通道最多 `KCP_CLEANUP_WAITS` 秒冲刷 disconnect
        let deadline = Instant::now() + Duration::from_secs(KCP_CLEANUP_WAITS as u64);
        while Instant::now() < deadline {
            channel.update(now_ms()).await?;
            if channel.waiting_for_send().await == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    Ok(())
}
