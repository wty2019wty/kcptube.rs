// SPDX-License-Identifier: GPL-3.0-only

//! mux 模式：共享隧道池的建立、接入与事件分发。

use crate::ClientError;
use crate::handshake::establish_channel;
use crate::state::ClientState;
use crate::util::allocate_connection_id;
use kcptube_protocol::constant::HANDSHAKE_TIMEOUT;
use kcptube_protocol::packet::{ProtocolType, mux_tell_server_connect_address};
use kcptube_transport::{
    MuxEvent, MuxSink, MuxTcpSink, MuxTunnel, TcpListenerSession, TcpSession, UdpEndpoint,
    is_connection_reset,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

/// mux 模式下的 TCP 接入：复用已建立的通道，按 `connection_id` 分流。
pub(crate) async fn mux_accept_loop(
    listener: TcpListenerSession,
    state: Arc<ClientState>,
    remote: Option<Arc<(String, u16)>>,
) -> Result<(), ClientError> {
    loop {
        match listener.accept().await {
            Ok((session, peer)) => {
                let state = state.clone();
                let remote = remote.clone();
                // 隧道可能暂时不足（启动瞬间或全部断开）：让连接排队等待，
                // 而不是立即关闭；等待超时后由 wait_mux_tunnel 关闭 session。
                tokio::spawn(async move {
                    if let Some(tunnel) = wait_mux_tunnel(&state, peer).await {
                        mux_handle_tcp(tunnel, session, remote).await;
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

/// 等待一条可用的 mux 隧道，最长 `HANDSHAKE_TIMEOUT` 秒。
///
/// 客户端会先监听、后建链，因此启动瞬间或所有通道暂时断开时，
/// 本地连接应排队等待，而不是被直接关闭。
async fn wait_mux_tunnel(state: &Arc<ClientState>, peer: SocketAddr) -> Option<Arc<MuxTunnel>> {
    let deadline = Instant::now() + Duration::from_secs(HANDSHAKE_TIMEOUT as u64);
    let mut logged = false;

    loop {
        if let Some(tunnel) = state.pick_mux_tunnel().await {
            return Some(tunnel);
        }
        if Instant::now() >= deadline {
            tracing::warn!("等待 mux 通道超时，关闭本地连接 {peer}");
            return None;
        }
        if !logged {
            tracing::info!("mux 通道尚未就绪，本地连接 {peer} 排队等待");
            logged = true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 一条本地 TCP 连接在 mux 通道上的转发生命周期。
async fn mux_handle_tcp(
    tunnel: Arc<MuxTunnel>,
    tcp: TcpSession,
    remote: Option<Arc<(String, u16)>>,
) {
    let connection_id = allocate_connection_id(&tunnel);
    let (mut reader, mut writer) = tcp.into_split();

    let (sink, mut data_rx, mut writer_closed) = MuxTcpSink::new(64);
    let record = tunnel.add_record(connection_id, ProtocolType::Tcp, MuxSink::Tcp(sink.clone()));

    // `[custom_input*]` 映射：先把目标地址告诉服务端，让它在数据到达前
    // 就开始连接（对应 C++ `mux_tell_server_connect_address`）。
    if let Some((host, port)) = remote.as_deref() {
        let packet = mux_tell_server_connect_address(ProtocolType::Tcp, connection_id, host, *port);
        if !tunnel.send_packet(packet).await {
            record.close();
            let _ = tunnel.remove_record(connection_id);
            return;
        }
    }

    let writer_task = tokio::spawn(async move {
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
        let _ = writer.shutdown().await;
    });

    let mut reader_closed = sink.subscribe();
    let mut buffer = vec![0u8; 8192];
    loop {
        tokio::select! {
            result = reader.read(&mut buffer) => {
                match result {
                    Ok(0) | Err(_) => break,
                    Ok(size) => {
                        record.touch();
                        let sent = tunnel
                            .send(ProtocolType::Tcp, connection_id, &buffer[..size])
                            .await;
                        if !sent {
                            break;
                        }
                    }
                }
            }
            _ = reader_closed.changed() => break,
        }
    }

    record.close();
    // 只有本地先结束时才通知服务端；被对端取消时记录已不在表里
    if tunnel.remove_record(connection_id).is_some() {
        let _ = tunnel.send_cancel(ProtocolType::Tcp, connection_id).await;
    }
    let _ = writer_task.await;
}

/// mux 模式下的本地 UDP 监听：每个源地址对应一条 mux 记录。
pub(crate) async fn mux_udp_listen_loop(
    listener: UdpEndpoint,
    state: Arc<ClientState>,
    remote: Option<Arc<(String, u16)>>,
) -> Result<(), ClientError> {
    let socket = Arc::new(listener);
    let mut buffer = vec![0u8; 65536];
    loop {
        let (size, peer) = match socket.recv_from(&mut buffer).await {
            Ok((size, peer)) => (size, peer),
            Err(error) if is_connection_reset(&error) => continue,
            Err(error) => {
                tracing::warn!("UDP 监听接收失败：{error}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        state.counters.add_ingress_raw(size);

        if let Some(record) = state.find_udp_record(peer).await {
            record.touch();
            if let Some(tunnel) = record.tunnel() {
                tunnel.try_send(
                    ProtocolType::Udp,
                    record.connection_id,
                    buffer[..size].to_vec(),
                );
            }
            continue;
        }

        let Some(tunnel) = state.pick_mux_tunnel().await else {
            continue;
        };
        let connection_id = allocate_connection_id(&tunnel);
        let sink = MuxSink::Udp {
            socket: socket.clone(),
            peer,
        };
        let record = tunnel.add_record(connection_id, ProtocolType::Udp, sink);
        record.touch();
        // 与 TCP 分支一致：先告诉服务端目标地址，再发送首个数据报。
        if let Some((host, port)) = remote.as_deref() {
            let packet =
                mux_tell_server_connect_address(ProtocolType::Udp, connection_id, host, *port);
            if !tunnel.send_packet(packet).await {
                record.close();
                let _ = tunnel.remove_record(connection_id);
                continue;
            }
        }
        let _ = tunnel.try_send(ProtocolType::Udp, connection_id, buffer[..size].to_vec());
    }
}

/// 客户端 mux 事件循环：把对端数据写回本地连接。
pub(crate) async fn mux_event_loop(tunnel: Arc<MuxTunnel>, mut events: mpsc::Receiver<MuxEvent>) {
    while let Some(event) = events.recv().await {
        match event {
            MuxEvent::Transfer {
                protocol,
                connection_id,
                payload,
            } => {
                let Some(record) = tunnel.record(connection_id) else {
                    // 本地连接已不存在，通知对端取消（对应 C++ `send_cancel_packet`）
                    let _ = tunnel.send_cancel(protocol, connection_id).await;
                    continue;
                };
                if record.protocol != protocol {
                    continue;
                }
                record.touch();
                if !record.sink.deliver(&payload).await {
                    record.close();
                    let _ = tunnel.remove_record(connection_id);
                    let _ = tunnel.send_cancel(protocol, connection_id).await;
                }
            }
            MuxEvent::Cancel { connection_id, .. } => {
                if let Some(record) = tunnel.remove_record(connection_id) {
                    record.close();
                }
            }
            MuxEvent::PreConnect { .. } => {}
        }
    }
    tunnel.clear_records();
}

/// 维持 `target` 条 mux 共享通道。
///
/// 与 C++ `establish_mux_channels` 一致，建链在后台异步进行：
/// 服务端暂时不可达时保持重试，不会让客户端退出；任何通道断开后
/// 也会由本任务补建。
pub(crate) async fn maintain_mux_tunnels(state: Arc<ClientState>, target: usize) {
    loop {
        let missing = {
            let mux = state.mux.lock().await;
            target.saturating_sub(mux.len())
        };

        if missing == 0 {
            // 数量正常时低频巡检，发现断开后在下一轮补建
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        match establish_channel(&state, ProtocolType::Mux, None).await {
            Ok((conv, channel)) => {
                let mut mux = state.mux.lock().await;
                if mux.len() >= target {
                    // 建链期间已有其它通道补位，丢弃本次多余链路
                    continue;
                }
                let tunnel = state.spawn_mux_tunnel(conv, channel);
                mux.push(tunnel);
                tracing::info!("mux 通道 conv={conv} 已建立");
            }
            Err(error) => {
                tracing::warn!("建立 mux 通道失败：{error}，2 秒后重试");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}
