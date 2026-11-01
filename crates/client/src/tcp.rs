// SPDX-License-Identifier: GPL-3.0-only

//! 非 mux 模式下的本地 TCP 接入与转发。

use crate::ClientError;
use crate::handshake::establish_channel;
use crate::state::ClientState;
use kcptube_kcp::KcpError;
use kcptube_protocol::constant::{KCP_CLEANUP_WAITS, PACKET_LENGTH};
use kcptube_protocol::packet::{
    Feature, ProtocolType, create_data_packet, create_keep_alive_packet,
    create_keep_alive_response_packet, inform_disconnect_packet, unpack_inner,
};
use kcptube_transport::{
    KcpChannel, MAX_INNER_MESSAGE, TcpListenerSession, TcpSession, TransportError, now_ms, now_secs,
};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Duration, Instant, interval};

pub(crate) async fn accept_loop(
    listener: TcpListenerSession,
    state: Arc<ClientState>,
    remote: Option<Arc<(String, u16)>>,
) -> Result<(), ClientError> {
    loop {
        match listener.accept().await {
            Ok((session, peer)) => {
                tracing::debug!("接受本地 TCP 连接 {peer}");
                let state = state.clone();
                let remote = remote.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(state, session, remote).await {
                        tracing::warn!("TCP 连接 {peer} 结束：{error}");
                    }
                });
            }
            Err(error) => {
                tracing::warn!("TCP accept 失败：{error}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// 一条本地 TCP 连接对应一条 KCP 通道。
async fn handle_connection(
    state: Arc<ClientState>,
    tcp: TcpSession,
    remote: Option<Arc<(String, u16)>>,
) -> Result<(), ClientError> {
    let (conv, channel) = establish_channel(&state, ProtocolType::Tcp, remote.as_deref()).await?;
    let id = state.register_channel(conv, channel.clone()).await;
    let result = relay_tcp(&state, tcp, channel).await;
    state.unregister_channel(id).await;
    result
}

async fn relay_tcp(
    state: &Arc<ClientState>,
    tcp: TcpSession,
    channel: Arc<KcpChannel>,
) -> Result<(), ClientError> {
    let (mut reader, mut writer) = tcp.into_split();
    let mut ticker = interval(Duration::from_millis(5));
    let mut tcp_buffer = vec![0u8; 2048];
    let mut datagram = vec![0u8; PACKET_LENGTH + 256];
    let mut inner_buffer = vec![0u8; MAX_INNER_MESSAGE];

    let keep_alive = i64::from(state.settings.keep_alive);
    let mut next_keep_alive = now_secs() + keep_alive;
    let mut remote_closed = false;

    'relay: loop {
        let socket = channel.socket();
        tokio::select! {
            result = reader.read(&mut tcp_buffer) => {
                match result {
                    Ok(0) | Err(_) => {
                        let _ = channel.send(&inform_disconnect_packet(ProtocolType::Tcp)).await;
                        let _ = channel.update(now_ms()).await;
                        break;
                    }
                    Ok(size) => {
                        let packet = create_data_packet(ProtocolType::Tcp, &tcp_buffer[..size]);
                        state.counters.add_egress_inner(size);
                        channel.send(&packet).await?;
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
                if keep_alive > 0 && now >= next_keep_alive {
                    let packet = create_keep_alive_packet(ProtocolType::Tcp);
                    channel.send(&packet).await?;
                    channel.mark_keep_alive_sent(now);
                    next_keep_alive = now + keep_alive;
                }
                channel.update(now_ms()).await?;
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
                Feature::RawData if protocol == ProtocolType::Tcp => {
                    if writer.write_all(body).await.is_err() {
                        break 'relay;
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
                    break 'relay;
                }
                Feature::Disconnect => {
                    remote_closed = true;
                    break 'relay;
                }
                _ => {}
            }
        }
    }

    if remote_closed {
        return Ok(());
    }

    // 本地 TCP 已结束：保留通道最多 `KCP_CLEANUP_WAITS` 秒冲刷最后的 disconnect
    let deadline = Instant::now() + Duration::from_secs(KCP_CLEANUP_WAITS as u64);
    while Instant::now() < deadline {
        channel.update(now_ms()).await?;
        if channel.waiting_for_send().await == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}
