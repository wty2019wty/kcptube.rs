// SPDX-License-Identifier: GPL-3.0-only

//! 服务端 mux 事件循环与本地目标连接。

use crate::ServerError;
use crate::state::{Destination, SocketState};
use kcptube_protocol::packet::{ProtocolType, inform_mux_cancel_packet};
use kcptube_transport::{
    MuxEvent, MuxRecord, MuxSink, MuxTcpSink, MuxTunnel, TcpSession, UdpEndpoint,
    is_connection_reset, resolve_endpoint_filtered, wildcard_addr,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// 服务端 mux 事件循环：把对端数据落到本地目标，并维护 `connection_id` 映射。
pub(crate) async fn mux_server_event_loop(
    state: Arc<SocketState>,
    tunnel: Arc<MuxTunnel>,
    default_target: Option<Destination>,
    mut events: mpsc::Receiver<MuxEvent>,
) {
    while let Some(event) = events.recv().await {
        match event {
            MuxEvent::Transfer {
                protocol,
                connection_id,
                payload,
            } => {
                let record = match tunnel.record(connection_id) {
                    Some(record) if record.protocol == protocol => record,
                    _ => {
                        // 已取消的连接再来数据：回 `mux_cancel` 让客户端放弃
                        if tunnel.is_expiring(connection_id) {
                            let _ = tunnel.send_cancel(protocol, connection_id).await;
                            continue;
                        }
                        // 对应 C++：`ignore_destination_*` 下 `create_mux_data_*`
                        // 拿不到客户端地址时返回 nullptr，上层回 `mux_cancel`。
                        let Some(target) = default_target.as_ref() else {
                            let _ = tunnel.send_cancel(protocol, connection_id).await;
                            continue;
                        };
                        match create_mux_local(&state, &tunnel, protocol, connection_id, target)
                            .await
                        {
                            Ok(record) => record,
                            Err(error) => {
                                tracing::warn!(
                                    "mux 连接 {connection_id}（{protocol:?}）建立失败：{error}"
                                );
                                let _ = tunnel.send_cancel(protocol, connection_id).await;
                                continue;
                            }
                        }
                    }
                };

                record.touch();
                if !record.sink.deliver(&payload).await {
                    record.close();
                    let _ = tunnel.remove_record(connection_id);
                    tunnel.mark_expiring(connection_id);
                    let _ = tunnel.send_cancel(protocol, connection_id).await;
                }
            }
            MuxEvent::Cancel { connection_id, .. } => {
                if let Some(record) = tunnel.remove_record(connection_id) {
                    record.close();
                    tunnel.mark_expiring(connection_id);
                }
            }
            MuxEvent::PreConnect { protocol, address } => {
                let connection_id = address.connection_id;
                if tunnel.record(connection_id).is_some() {
                    continue;
                }
                if tunnel.is_expiring(connection_id) {
                    let packet = inform_mux_cancel_packet(protocol, connection_id);
                    let _ = tunnel.send_packet(packet).await;
                    continue;
                }
                let (host, port) = (address.user_input_ip.clone(), address.user_input_port);
                match resolve_endpoint_filtered(
                    &host,
                    port,
                    state.settings.ip_version_only.want_ipv4(),
                )
                .await
                {
                    Ok(resolved) => {
                        let target = Destination {
                            host: host.clone(),
                            port,
                            address: resolved,
                        };
                        match create_mux_local(&state, &tunnel, protocol, connection_id, &target)
                            .await
                        {
                            Ok(_) => {}
                            Err(error) => {
                                tracing::warn!("mux 预连接 {host}:{port} 失败：{error}");
                                let packet = inform_mux_cancel_packet(protocol, connection_id);
                                let _ = tunnel.send_packet(packet).await;
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!("mux 预连接地址 {host}:{port} 解析失败：{error}");
                        let packet = inform_mux_cancel_packet(protocol, connection_id);
                        let _ = tunnel.send_packet(packet).await;
                    }
                }
            }
        }
    }
    tunnel.clear_records();
}

/// 为一条 mux 记录建立本地目标连接，并启动「目标 → 隧道」的回程读取任务。
async fn create_mux_local(
    state: &Arc<SocketState>,
    tunnel: &Arc<MuxTunnel>,
    protocol: ProtocolType,
    connection_id: u32,
    target: &Destination,
) -> Result<Arc<MuxRecord>, ServerError> {
    match protocol {
        ProtocolType::Tcp => {
            let session = TcpSession::connect(target.address).await?;
            tracing::debug!(
                "mux 连接 {connection_id} -> TCP {}:{}",
                target.host,
                target.port
            );
            let (mut reader, mut writer) = session.into_split();
            let (sink, mut data_rx, mut writer_closed) = MuxTcpSink::new(64);
            let record =
                tunnel.add_record(connection_id, ProtocolType::Tcp, MuxSink::Tcp(sink.clone()));

            let writer_record = record.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        data = data_rx.recv() => {
                            let Some(data) = data else { break };
                            if writer.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                        _ = writer_closed.changed() => break,
                    }
                }
                writer_record.close();
                let _ = writer.shutdown().await;
            });

            let reader_tunnel = tunnel.clone();
            let reader_record = record.clone();
            let mut reader_closed = sink.subscribe();
            let counters = state.counters.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 8192];
                loop {
                    tokio::select! {
                        result = reader.read(&mut buffer) => {
                            match result {
                                Ok(0) | Err(_) => break,
                                Ok(size) => {
                                    reader_record.touch();
                                    counters.add_egress_inner(size);
                                    if !reader_tunnel
                                        .send(ProtocolType::Tcp, connection_id, &buffer[..size])
                                        .await
                                    {
                                        break;
                                    }
                                }
                            }
                        }
                        _ = reader_closed.changed() => break,
                    }
                }
                reader_record.close();
                if reader_tunnel.remove_record(connection_id).is_some() {
                    let _ = reader_tunnel
                        .send_cancel(ProtocolType::Tcp, connection_id)
                        .await;
                }
            });

            Ok(record)
        }
        ProtocolType::Udp => {
            let bind: SocketAddr = wildcard_addr(target.address);
            let socket = Arc::new(UdpEndpoint::bind(bind).await?);
            let record = tunnel.add_record(
                connection_id,
                ProtocolType::Udp,
                MuxSink::Udp {
                    socket: socket.clone(),
                    peer: target.address,
                },
            );

            let reader_tunnel = tunnel.clone();
            let reader_record = record.clone();
            let counters = state.counters.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 65536];
                loop {
                    let (size, _) = match socket.recv_from(&mut buffer).await {
                        Ok(received) => received,
                        Err(error) if is_connection_reset(&error) => continue,
                        Err(_) => break,
                    };
                    reader_record.touch();
                    counters.add_egress_inner(size);
                    if !reader_tunnel
                        .send(ProtocolType::Udp, connection_id, &buffer[..size])
                        .await
                    {
                        break;
                    }
                }
                reader_record.close();
                if reader_tunnel.remove_record(connection_id).is_some() {
                    let _ = reader_tunnel
                        .send_cancel(ProtocolType::Udp, connection_id)
                        .await;
                }
            });

            Ok(record)
        }
        _ => Err(ServerError::Startup(format!(
            "mux 不支持的协议类型：{protocol:?}"
        ))),
    }
}
