// SPDX-License-Identifier: GPL-3.0-only

//! 本地目标到 KCP 通道的读取转发任务。

use crate::state::DataSession;
use kcptube_logging::StatusCounters;
use kcptube_protocol::constant::KEEPALIVE_TIMEOUT;
use kcptube_protocol::packet::{ProtocolType, create_data_packet, inform_disconnect_packet};
use kcptube_transport::{UdpEndpoint, is_connection_reset, now_secs};
use std::sync::Arc;
use tokio::io::AsyncReadExt;

pub(crate) async fn tcp_reader_loop(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    session: Arc<DataSession>,
    counters: Arc<StatusCounters>,
) {
    let mut buffer = vec![0u8; 2048];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(size) => {
                tracing::trace!("TCP 读到 {size} 字节，发送 raw_data");
                let packet = create_data_packet(ProtocolType::Tcp, &buffer[..size]);
                counters.add_egress_inner(size);
                if session.channel.send(&packet).await.is_err() {
                    break;
                }
            }
        }
    }

    // 本地 TCP 已关闭：通知对端，并保留通道 `KEEPALIVE_TIMEOUT` 秒用于冲刷重传
    // （对应 C++ `expiring_kcp.insert({..., right_now() + gbv_keepalive_timeout})`）
    if !session.is_closed() {
        let _ = session
            .channel
            .send(&inform_disconnect_packet(ProtocolType::Tcp))
            .await;
        session.begin_close(now_secs() + KEEPALIVE_TIMEOUT);
    }
}

pub(crate) async fn udp_relay_loop(
    socket: UdpEndpoint,
    session: Arc<DataSession>,
    counters: Arc<StatusCounters>,
) {
    let mut buffer = vec![0u8; 65536];
    loop {
        match socket.recv_from(&mut buffer).await {
            Ok((size, _peer)) => {
                if session.is_closed() {
                    break;
                }
                session.mark_receive(now_secs());
                let packet = create_data_packet(ProtocolType::Udp, &buffer[..size]);
                counters.add_egress_inner(size);
                if session.channel.send(&packet).await.is_err() {
                    break;
                }
            }
            Err(error) if is_connection_reset(&error) => continue,
            Err(_) => break,
        }
    }

    if !session.is_closed() {
        let _ = session
            .channel
            .send(&inform_disconnect_packet(ProtocolType::Udp))
            .await;
        session.begin_close(now_secs() + KEEPALIVE_TIMEOUT);
    }
}
