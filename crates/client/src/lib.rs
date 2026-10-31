// SPDX-License-Identifier: GPL-3.0-only

//! 客户端模式（对应 C++ `modes/client.cpp`）。
//!
//! M3 范围：本地 TCP 监听、`initialise` 握手（超时后换新端口重试）、
//! 单通道 `raw_data` 转发、保活与状态日志。
//! 端口跳跃、mux、STUN、FEC 留到后续里程碑。

use kcptube_config::UserSettings;
use kcptube_kcp::{KcpConfig, KcpError};
use kcptube_logging::{
    LOGGING_GAP_SECONDS, StatusCounters, print_message_to_file, print_status_to_file,
    time_string_with_brackets, to_speed_unit,
};
use kcptube_protocol::ProtocolError;
use kcptube_protocol::constant::{HANDSHAKE_TIMEOUT, KCP_CLEANUP_WAITS, PACKET_LENGTH};
use kcptube_protocol::packet::{
    Feature, ProtocolType, SettingsWrapper, create_data_packet, create_keep_alive_packet,
    create_keep_alive_response_packet, inform_disconnect_packet, request_initialise_packet,
    unpack_inner,
};
use kcptube_transport::{
    KcpChannel, MAX_INNER_MESSAGE, PacketCodec, TcpListenerSession, TcpSession, TransportError,
    UdpEndpoint, now_ms, now_secs, resolve_endpoint,
};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant, interval};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("客户端启动失败：{0}")]
    Startup(String),
    #[error("传输层错误：{0}")]
    Transport(#[from] TransportError),
    #[error("协议错误：{0}")]
    Protocol(#[from] ProtocolError),
    #[error("KCP 错误：{0}")]
    Kcp(#[from] KcpError),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("握手超时")]
    HandshakeTimeout,
    #[error("功能尚未实现：{0}")]
    NotImplemented(&'static str),
}

pub struct Client {
    settings: UserSettings,
}

impl Client {
    pub fn new(settings: UserSettings) -> Self {
        Self { settings }
    }

    pub fn settings(&self) -> &UserSettings {
        &self.settings
    }

    /// 启动客户端（阻塞直到退出）
    pub async fn run(&self) -> Result<(), ClientError> {
        let settings = Arc::new(self.settings.clone());
        let codec = Arc::new(PacketCodec::new(
            settings.encryption,
            &settings.encryption_password,
        ));

        if settings.listen_ports.is_empty() {
            return Err(ClientError::Startup(
                "listen_port is not set（自定义输入映射将在 M4 实现）".to_string(),
            ));
        }

        let server_host = settings
            .destination_address_list
            .first()
            .cloned()
            .ok_or_else(|| ClientError::Startup("destination_address is not set".to_string()))?;
        let server_port = *settings
            .destination_ports
            .first()
            .ok_or_else(|| ClientError::Startup("destination_port is not set".to_string()))?;
        let server_peer = resolve_endpoint(&server_host, server_port).await?;
        tracing::info!("客户端目标服务端 {server_peer}");

        let listen_addresses = if settings.listen_on.is_empty() {
            vec!["0.0.0.0".to_string()]
        } else {
            settings.listen_on.clone()
        };

        let state = Arc::new(ClientState {
            settings: settings.clone(),
            codec: codec.clone(),
            fec: fec_pair(&settings),
            counters: Arc::new(StatusCounters::default()),
            channels: Mutex::new(HashMap::new()),
            udp_relays: Mutex::new(HashMap::new()),
            next_channel_id: AtomicU64::new(1),
            next_status_at: AtomicI64::new(now_secs() + LOGGING_GAP_SECONDS as i64),
        });

        let mut tasks = JoinSet::new();

        // 状态摘要任务：只有配置了 `log_status`（即设置了 `log_path`）时才启动，
        // 与 C++ `if (!current_settings.log_status.empty())` 一致
        if !state.settings.log_status.as_os_str().is_empty() {
            let state = state.clone();
            tasks.spawn(async move {
                let mut ticker = interval(Duration::from_secs(1));
                loop {
                    ticker.tick().await;
                    state.maybe_log_status().await;
                }
            });
        }

        for address in listen_addresses {
            let ip: IpAddr = address
                .parse()
                .map_err(|_| ClientError::Startup(format!("无效的 listen_on 地址：{address}")))?;
            for port in &settings.listen_ports {
                let addr = SocketAddr::new(ip, *port);
                let listener = match TcpListenerSession::bind(addr).await {
                    Ok(listener) => listener,
                    Err(error) => {
                        let line = format!(
                            "{}Listen Address incorrect - {addr}\n",
                            time_string_with_brackets()
                        );
                        let _ = print_message_to_file(&settings.log_messages, &line);
                        return Err(ClientError::Startup(format!(
                            "无法监听 TCP {addr}: {error}"
                        )));
                    }
                };
                tracing::info!("客户端监听 TCP {addr}");

                let tcp_state = state.clone();
                let host = server_host.clone();
                tasks.spawn(
                    async move { accept_loop(listener, tcp_state, host, server_port).await },
                );

                // 同一端口同时监听 UDP（对应 C++ 的 `udp_access_points`）
                let udp_listener = match UdpEndpoint::bind(addr).await {
                    Ok(endpoint) => endpoint,
                    Err(error) => {
                        let line = format!(
                            "{}Listen Address incorrect - {addr}\n",
                            time_string_with_brackets()
                        );
                        let _ = print_message_to_file(&settings.log_messages, &line);
                        return Err(ClientError::Startup(format!(
                            "无法监听 UDP {addr}: {error}"
                        )));
                    }
                };
                tracing::info!("客户端监听 UDP {addr}");

                let udp_state = state.clone();
                let host = server_host.clone();
                tasks.spawn(async move {
                    udp_listen_loop(udp_listener, udp_state, host, server_port).await
                });
            }
        }

        if tasks.is_empty() {
            return Err(ClientError::Startup("没有可用的监听地址".to_string()));
        }

        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error),
                Err(error) => {
                    return Err(ClientError::Startup(format!("监听任务崩溃：{error}")));
                }
            }
        }
        Ok(())
    }
}

struct ClientState {
    settings: Arc<UserSettings>,
    codec: Arc<PacketCodec>,
    /// FEC 配置 `(k, r)`；未启用时为 `None`
    fec: Option<(u8, u8)>,
    counters: Arc<StatusCounters>,
    /// 活跃的数据通道（conv + 通道，仅用于状态日志）
    channels: Mutex<HashMap<u64, (u32, Arc<KcpChannel>)>>,
    /// 本地 UDP 源地址 → 该地址对应中继的入队通道
    udp_relays: Mutex<HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>>,
    next_channel_id: AtomicU64,
    next_status_at: AtomicI64,
}

impl ClientState {
    /// 登记一条数据通道，返回的 id 用于结束时注销
    async fn register_channel(&self, conv: u32, channel: Arc<KcpChannel>) -> u64 {
        let id = self.next_channel_id.fetch_add(1, Ordering::Relaxed);
        self.channels.lock().await.insert(id, (conv, channel));
        id
    }

    async fn unregister_channel(&self, id: u64) {
        self.channels.lock().await.remove(&id);
    }

    fn log_message(&self, message: &str) {
        let line = format!("{}{message}\n", time_string_with_brackets());
        tracing::warn!("{}", message);
        let _ = print_message_to_file(&self.settings.log_messages, &line);
    }

    async fn maybe_log_status(&self) {
        let now = now_secs();
        if now < self.next_status_at.load(Ordering::SeqCst) {
            return;
        }
        self.next_status_at
            .store(now + LOGGING_GAP_SECONDS as i64, Ordering::SeqCst);

        let duration = LOGGING_GAP_SECONDS;
        let receive_raw = to_speed_unit(
            StatusCounters::take(&self.counters.ingress_raw_traffic),
            duration,
        );
        let send_inner = to_speed_unit(
            StatusCounters::take(&self.counters.egress_inner_traffic),
            duration,
        );

        let channels: Vec<(u32, Arc<KcpChannel>)> = self
            .channels
            .lock()
            .await
            .values()
            .map(|(conv, channel)| (*conv, channel.clone()))
            .collect();
        let mut receive_inner_bytes = 0u64;
        let mut send_raw_bytes = 0u64;
        let mut latencies = Vec::new();
        for (conv, channel) in &channels {
            receive_inner_bytes += channel.take_input_bytes();
            send_raw_bytes += channel.take_output_bytes();
            latencies.push((*conv, channel.rx_srtt().await));
        }
        let receive_inner = to_speed_unit(receive_inner_bytes, duration);
        let send_raw = to_speed_unit(send_raw_bytes, duration);
        let fec_recover = StatusCounters::take(&self.counters.fec_recovery_count);

        let mut output = format!(
            "{}Summary of {}\n",
            time_string_with_brackets(),
            self.settings.config_filename.display()
        );
        output.push_str(&format!(
            "receive (raw): {receive_raw}, receive (inner): {receive_inner}, send (inner): {send_inner}, send (raw): {send_raw}, fec recover: {fec_recover}\n"
        ));
        for (conv, srtt) in &latencies {
            output.push_str(&format!("KCP#{conv} average latency: {srtt} ms\n"));
        }
        output.push('\n');

        let _ = print_status_to_file(&self.settings.log_status, &output);
        // 对应 C++ `std::cout << output_text << std::endl;`（末尾再补一个换行）
        println!("{output}");
    }
}

/// 本地 UDP 监听：按源地址把数据分发给各自的中继任务。
///
/// 对应 C++ `client_mode::udp_listener_incoming`：每个本地源地址
/// （`udp_local_session_map_to_kcp` 的键）对应一条独立的 KCP 通道。
async fn udp_listen_loop(
    listener: UdpEndpoint,
    state: Arc<ClientState>,
    server_host: String,
    server_port: u16,
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
        let host = server_host.clone();
        tokio::spawn(async move {
            if let Err(error) =
                udp_relay_task(state.clone(), listener, peer, host, server_port, receiver).await
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
    server_host: String,
    server_port: u16,
    from_local: mpsc::Receiver<Vec<u8>>,
) -> Result<(), ClientError> {
    let (conv, channel) =
        establish_channel(&state, &server_host, server_port, ProtocolType::Udp).await?;
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

    let keep_alive = i64::from(state.settings.keep_alive);
    let mut next_keep_alive = now_secs() + keep_alive;
    let udp_timeout = i64::from(state.settings.udp_timeout);
    let mut last_local = now_secs();
    let mut last_remote = now_secs();
    let mut remote_closed = false;

    loop {
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
            result = channel.socket.recv_from(&mut datagram) => {
                if let Ok((size, _)) = result {
                    state.counters.add_ingress_raw(size);
                    let _ = channel.process_datagram(&datagram[..size]).await;
                }
            }
            _ = ticker.tick() => {
                let now = now_secs();
                if keep_alive > 0 && now >= next_keep_alive {
                    let packet = create_keep_alive_packet(ProtocolType::Udp);
                    channel.send(&packet).await?;
                    channel.mark_keep_alive_sent(now);
                    next_keep_alive = now + keep_alive;
                }
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

/// Windows 上 UDP 对端不可达会以「连接重置/中止」的形式返回（WSAECONNRESET）。
fn is_connection_reset(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
    )
}

async fn accept_loop(
    listener: TcpListenerSession,
    state: Arc<ClientState>,
    server_host: String,
    server_port: u16,
) -> Result<(), ClientError> {
    loop {
        match listener.accept().await {
            Ok((session, peer)) => {
                tracing::debug!("接受本地 TCP 连接 {peer}");
                let state = state.clone();
                let host = server_host.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(state, host, server_port, session).await {
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
    server_host: String,
    server_port: u16,
    tcp: TcpSession,
) -> Result<(), ClientError> {
    let (conv, channel) =
        establish_channel(&state, &server_host, server_port, ProtocolType::Tcp).await?;
    let id = state.register_channel(conv, channel.clone()).await;
    let result = relay_tcp(&state, tcp, channel).await;
    state.unregister_channel(id).await;
    result
}

/// 建立一条数据通道（含握手与 30 秒超时换端口重试）。
///
/// 与 C++ `create_handshake` + `handshake_timeout_detection` 的行为一致：
/// 超时后丢弃旧 socket，重新解析服务端地址并发起新握手。
async fn establish_channel(
    state: &Arc<ClientState>,
    server_host: &str,
    server_port: u16,
    protocol: ProtocolType,
) -> Result<(u32, Arc<KcpChannel>), ClientError> {
    loop {
        let server_peer = resolve_endpoint(server_host, server_port).await?;
        let bind_addr = if server_peer.is_ipv4() {
            SocketAddr::from(([0, 0, 0, 0], 0))
        } else {
            SocketAddr::from(([0u16; 8], 0))
        };
        let socket = UdpEndpoint::bind(bind_addr).await?;

        let handshake = Arc::new(
            KcpChannel::new_with_fec(
                state.codec.clone(),
                handshake_config(state.settings.kcp_mtu),
                socket.clone(),
                server_peer,
                state.fec,
            )
            .await?,
        );
        {
            let mut kcp = handshake.endpoint.kcp.lock().await;
            kcp.set_rx_minrto(10);
        }

        let packet = request_initialise_packet(
            protocol,
            state.settings.outbound_bandwidth,
            state.settings.inbound_bandwidth,
        );
        handshake.send(&packet).await?;
        handshake.update(now_ms()).await?;

        match wait_handshake(state, &handshake, &socket).await {
            Ok(response) => {
                let conv = response.uid;
                if conv == 0 {
                    state.log_message("服务端返回的 conv 为 0，重新握手");
                    continue;
                }
                tracing::debug!("握手完成，conv={conv}");

                let channel = Arc::new(
                    KcpChannel::new_with_fec(
                        state.codec.clone(),
                        data_config(&state.settings, conv),
                        socket.clone(),
                        server_peer,
                        state.fec,
                    )
                    .await?,
                );
                {
                    let mut kcp = channel.endpoint.kcp.lock().await;
                    kcp.set_rx_minrto(10);
                    kcp.update(now_ms());
                }
                return Ok((conv, channel));
            }
            Err(ClientError::HandshakeTimeout) => {
                state.log_message(&format!(
                    "握手超时 {HANDSHAKE_TIMEOUT} 秒，切换新的本地端口重试"
                ));
                continue;
            }
            Err(ClientError::Startup(message)) => {
                state.log_message(&message);
                return Err(ClientError::Startup(message));
            }
            Err(error) => return Err(error),
        }
    }
}

async fn wait_handshake(
    state: &Arc<ClientState>,
    channel: &Arc<KcpChannel>,
    socket: &UdpEndpoint,
) -> Result<SettingsWrapper, ClientError> {
    let deadline = Instant::now() + Duration::from_secs(HANDSHAKE_TIMEOUT as u64);
    let mut ticker = interval(Duration::from_millis(5));
    let mut datagram = vec![0u8; PACKET_LENGTH + 256];
    let mut inner_buffer = vec![0u8; MAX_INNER_MESSAGE];

    loop {
        if Instant::now() >= deadline {
            return Err(ClientError::HandshakeTimeout);
        }

        tokio::select! {
            _ = ticker.tick() => {
                channel.update(now_ms()).await?;
            }
            result = socket.recv_from(&mut datagram) => {
                let (size, _) = result?;
                state.counters.add_ingress_raw(size);
                if channel.process_datagram(&datagram[..size]).await.is_err() {
                    continue;
                }
                loop {
                    let size = match channel.receive(&mut inner_buffer).await {
                        Ok(size) => size,
                        Err(TransportError::Kcp(KcpError::WouldBlock)) => break,
                        Err(error) => return Err(error.into()),
                    };
                    let (feature, _protocol, body) = unpack_inner(&inner_buffer[..size])?;
                    match feature {
                        Feature::Initialise => {
                            let settings = SettingsWrapper::decode(body)?;
                            if settings.uid != 0 {
                                return Ok(settings);
                            }
                        }
                        Feature::Failure => {
                            return Err(ClientError::Startup(format!(
                                "failure, error message: {}",
                                String::from_utf8_lossy(body)
                            )));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
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
            result = channel.socket.recv_from(&mut datagram) => {
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

/// FEC 配置：只有 k 与 r 都大于 0 时才启用。
fn fec_pair(settings: &UserSettings) -> Option<(u8, u8)> {
    let original = settings.fec_original_packet_count;
    let redundant = settings.fec_redundant_packet_count;
    (original > 0 && redundant > 0).then_some((original, redundant))
}

fn handshake_config(mtu: i32) -> KcpConfig {
    KcpConfig {
        conv: 0,
        mtu: mtu.max(50) as usize,
        nodelay: 1,
        interval: 1,
        resend: 3,
        no_congestion_control: true,
        send_window: 32,
        receive_window: 128,
        stream_mode: false,
    }
}

fn data_config(settings: &UserSettings, conv: u32) -> KcpConfig {
    KcpConfig {
        conv,
        mtu: settings.kcp_mtu.max(50) as usize,
        nodelay: settings.kcp_nodelay,
        interval: settings.kcp_interval.max(1) as u32,
        resend: settings.kcp_resend.max(0) as u32,
        no_congestion_control: settings.kcp_nc != 0,
        send_window: settings.kcp_sndwnd,
        receive_window: settings.kcp_rcvwnd,
        stream_mode: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_timeout_matches_cpp() {
        assert_eq!(HANDSHAKE_TIMEOUT, 30);
    }
}
