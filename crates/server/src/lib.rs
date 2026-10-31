// SPDX-License-Identifier: GPL-3.0-only

//! 服务端模式（对应 C++ `modes/server.cpp`）。
//!
//! 已实现：UDP 监听、`initialise` 握手、TCP/UDP/mux 转发、FEC、
//! 空闲清理、保活与状态日志。
//! 端口范围监听、端口跳跃、STUN/DDNS 与 `[custom_input]` 映射留到后续里程碑。

use kcptube_config::UserSettings;
use kcptube_kcp::{KcpConfig, KcpError};
use kcptube_logging::{
    LOGGING_GAP_SECONDS, StatusCounters, print_message_to_file, print_status_to_file,
    time_string_with_brackets, to_speed_unit,
};
use kcptube_protocol::ProtocolError;
use kcptube_protocol::constant::{
    KCP_CLEANUP_WAITS, KEEPALIVE_TIMEOUT, PACKET_LENGTH, TIME_GAP_SECONDS,
};
use kcptube_protocol::packet::{
    Feature, ProtocolType, SettingsWrapper, create_data_packet, create_keep_alive_packet,
    create_keep_alive_response_packet, inform_disconnect_packet, inform_error_packet,
    inform_mux_cancel_packet, response_initialise_packet, unpack_inner,
};
use kcptube_transport::{
    IncomingFrame, KcpChannel, MAX_INNER_MESSAGE, MuxEvent, MuxRecord, MuxSink, MuxTcpSink,
    MuxTunnel, PacketCodec, TcpSession, TransportError, UdpEndpoint, now_ms, now_secs, parse_frame,
    resolve_endpoint,
};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;
use tokio::time::{Duration, interval};

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("服务端启动失败：{0}")]
    Startup(String),
    #[error("传输层错误：{0}")]
    Transport(#[from] TransportError),
    #[error("协议错误：{0}")]
    Protocol(#[from] ProtocolError),
    #[error("KCP 错误：{0}")]
    Kcp(#[from] KcpError),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("功能尚未实现：{0}")]
    NotImplemented(&'static str),
}

pub struct Server {
    settings: UserSettings,
}

impl Server {
    pub fn new(settings: UserSettings) -> Self {
        Self { settings }
    }

    pub fn settings(&self) -> &UserSettings {
        &self.settings
    }

    /// 启动服务端（阻塞直到退出）
    pub async fn run(&self) -> Result<(), ServerError> {
        let settings = Arc::new(self.settings.clone());
        let codec = Arc::new(PacketCodec::new(
            settings.encryption,
            &settings.encryption_password,
        ));

        let port = *settings
            .listen_ports
            .first()
            .ok_or_else(|| ServerError::Startup("listen_port is not set".to_string()))?;
        if settings.listen_ports.len() > 1 {
            tracing::warn!("M3 只监听第一个端口 {}（端口范围将在 M4 实现）", port);
        }

        let listen_addresses = if settings.listen_on.is_empty() {
            vec!["0.0.0.0".to_string()]
        } else {
            settings.listen_on.clone()
        };

        let mut tasks = JoinSet::new();
        for address in listen_addresses {
            let ip: IpAddr = address
                .parse()
                .map_err(|_| ServerError::Startup(format!("无效的 listen_on 地址：{address}")))?;
            let addr = SocketAddr::new(ip, port);
            let socket = match UdpEndpoint::bind(addr).await {
                Ok(socket) => socket,
                Err(error) => {
                    let message = format!(
                        "{}Listen Address incorrect - {addr}\n",
                        time_string_with_brackets()
                    );
                    let _ = print_message_to_file(&settings.log_messages, &message);
                    return Err(ServerError::Startup(format!("无法监听 {addr}: {error}")));
                }
            };
            tracing::info!("服务端监听 UDP {addr}");

            let state = Arc::new(SocketState {
                settings: settings.clone(),
                codec: codec.clone(),
                fec: fec_pair(&settings),
                fec_original: fec_pair(&settings).map(|(original, _)| original as usize),
                handshakes: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                counters: Arc::new(StatusCounters::default()),
                next_status_at: AtomicI64::new(now_secs() + LOGGING_GAP_SECONDS as i64),
            });
            tasks.spawn(async move { state.run(socket).await });
        }

        if tasks.is_empty() {
            return Err(ServerError::Startup("没有可用的监听地址".to_string()));
        }

        // 任一监听循环异常退出就上报
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error),
                Err(error) => {
                    return Err(ServerError::Startup(format!("监听任务崩溃：{error}")));
                }
            }
        }
        Ok(())
    }
}

struct HandshakeEntry {
    channel: Arc<KcpChannel>,
    established: AtomicBool,
    /// 建链时刻（Unix 秒），用于回收握手通道
    created_at: i64,
}

/// 数据会话的落地端：TCP 写半边、UDP 目标或 mux 隧道。
enum SessionKind {
    Tcp(Arc<Mutex<OwnedWriteHalf>>),
    Udp {
        socket: UdpEndpoint,
        target: SocketAddr,
    },
    /// mux 会话：一条通道上复用多条本地连接
    Mux {
        tunnel: Arc<MuxTunnel>,
    },
}

struct DataSession {
    channel: Arc<KcpChannel>,
    kind: SessionKind,
    protocol: ProtocolType,
    closed: AtomicBool,
    /// 允许回收的时刻（Unix 秒）；`closed` 置位后才有效
    expire_at: AtomicI64,
    /// 下一次发送 `keep_alive` 的时刻（Unix 秒）
    next_keep_alive: AtomicI64,
    /// 最近一次向目标发送 / 从目标收到数据的时刻（Unix 秒）
    last_send_at: AtomicI64,
    last_receive_at: AtomicI64,
}

impl DataSession {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// 进入关闭流程：仍保留通道一段时间用于冲刷重传（对应 C++ `expiring_kcp`）。
    fn begin_close(&self, expire_at: i64) {
        self.closed.store(true, Ordering::SeqCst);
        self.expire_at.store(expire_at, Ordering::SeqCst);
    }

    fn expire_at(&self) -> i64 {
        self.expire_at.load(Ordering::SeqCst)
    }

    fn next_keep_alive(&self) -> i64 {
        self.next_keep_alive.load(Ordering::SeqCst)
    }

    fn set_next_keep_alive(&self, at: i64) {
        self.next_keep_alive.store(at, Ordering::SeqCst);
    }

    fn mark_send(&self, at: i64) {
        self.last_send_at.store(at, Ordering::SeqCst);
    }

    fn mark_receive(&self, at: i64) {
        self.last_receive_at.store(at, Ordering::SeqCst);
    }

    fn last_send_at(&self) -> i64 {
        self.last_send_at.load(Ordering::SeqCst)
    }

    fn last_receive_at(&self) -> i64 {
        self.last_receive_at.load(Ordering::SeqCst)
    }
}

/// 解析后的目标地址（用于日志与连接）
#[derive(Clone)]
struct Destination {
    host: String,
    port: u16,
    address: SocketAddr,
}

struct SocketState {
    settings: Arc<UserSettings>,
    codec: Arc<PacketCodec>,
    /// FEC 配置 `(k, r)`；未启用时为 `None`
    fec: Option<(u8, u8)>,
    /// FEC 原始包数量，用于解析帧类型
    fec_original: Option<usize>,
    handshakes: Mutex<HashMap<SocketAddr, Arc<HandshakeEntry>>>,
    sessions: Mutex<HashMap<u32, Arc<DataSession>>>,
    counters: Arc<StatusCounters>,
    /// 下一次输出状态摘要的时刻（Unix 秒）
    next_status_at: AtomicI64,
}

impl SocketState {
    async fn run(self: Arc<Self>, socket: UdpEndpoint) -> Result<(), ServerError> {
        let mut buffer = vec![0u8; PACKET_LENGTH + 256];
        // KCP 驱动：5ms 一次，与 C++ `KCPUpdater` 的调度精度一致
        let mut kcp_ticker = interval(Duration::from_millis(5));
        // 维护：每秒一次（对应 C++ `gbv_expring_update_interval`）
        let mut maintenance = interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                result = socket.recv_from(&mut buffer) => {
                    match result {
                        Ok((size, peer)) => {
                            if let Err(error) = self
                                .handle_datagram(&socket, peer, &buffer[..size])
                                .await
                            {
                                tracing::debug!("丢弃来自 {peer} 的数据报：{error}");
                            }
                        }
                        Err(error) if is_connection_reset(&error) => {
                            // Windows 上 UDP 收到 ICMP 端口不可达（对端已关闭）时会返回
                            // WSAECONNRESET；会话关闭后仍在重传尾包时很常见，忽略即可。
                            tracing::trace!("UDP 收到对端不可达提示，已忽略");
                        }
                        Err(error) => {
                            tracing::warn!("UDP 接收失败：{error}");
                        }
                    }
                }
                _ = kcp_ticker.tick() => {
                    self.tick_kcp().await;
                }
                _ = maintenance.tick() => {
                    self.maintain().await;
                    self.maybe_log_status().await;
                }
            }
        }
    }

    async fn handle_datagram(
        self: &Arc<Self>,
        socket: &UdpEndpoint,
        peer: SocketAddr,
        datagram: &[u8],
    ) -> Result<(), ServerError> {
        let (_, plain) = self.codec.open_frame(datagram)?;
        let frame = parse_frame(&plain, self.fec_original)?;
        let Some(conv) = frame.conv() else {
            return Ok(());
        };
        self.counters.add_ingress_raw(datagram.len());
        self.counters.add_ingress_inner(plain.len());

        if conv == 0 {
            self.handle_handshake(socket, peer, frame).await
        } else {
            self.handle_data(conv, peer, frame).await
        }
    }

    async fn handle_handshake(
        self: &Arc<Self>,
        socket: &UdpEndpoint,
        peer: SocketAddr,
        frame: IncomingFrame<'_>,
    ) -> Result<(), ServerError> {
        let entry = self.handshake_entry(socket, peer).await?;
        entry.channel.accept_frame(frame).await?;

        let mut buffer = vec![0u8; MAX_INNER_MESSAGE];
        loop {
            let size = match entry.channel.receive(&mut buffer).await {
                Ok(size) => size,
                Err(TransportError::Kcp(KcpError::WouldBlock)) => break,
                Err(error) => return Err(error.into()),
            };
            let (feature, protocol, body) = unpack_inner(&buffer[..size])?;
            match feature {
                Feature::Initialise => {
                    if entry.established.load(Ordering::SeqCst) {
                        continue;
                    }
                    let settings = SettingsWrapper::decode(body)?;
                    self.establish_session(socket, peer, &entry, protocol, settings)
                        .await?;
                    entry.established.store(true, Ordering::SeqCst);
                }
                Feature::KeepAlive => {
                    entry
                        .channel
                        .send(&create_keep_alive_response_packet(protocol))
                        .await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn handshake_entry(
        &self,
        socket: &UdpEndpoint,
        peer: SocketAddr,
    ) -> Result<Arc<HandshakeEntry>, ServerError> {
        if let Some(entry) = self.handshakes.lock().await.get(&peer) {
            return Ok(entry.clone());
        }

        let channel = Arc::new(
            KcpChannel::new_with_fec(
                self.codec.clone(),
                handshake_config(self.settings.kcp_mtu),
                socket.clone(),
                peer,
                self.fec,
            )
            .await?,
        );
        {
            let mut kcp = channel.endpoint.kcp.lock().await;
            kcp.set_rx_minrto(10);
        }
        let entry = Arc::new(HandshakeEntry {
            channel,
            established: AtomicBool::new(false),
            created_at: now_secs(),
        });
        self.handshakes.lock().await.insert(peer, entry.clone());
        Ok(entry)
    }

    /// 根据握手包里的协议类型建立 TCP 或 UDP 转发会话。
    async fn establish_session(
        self: &Arc<Self>,
        socket: &UdpEndpoint,
        peer: SocketAddr,
        entry: &HandshakeEntry,
        protocol: ProtocolType,
        wrapper: SettingsWrapper,
    ) -> Result<(), ServerError> {
        let (host, port) = match self.target_for(&wrapper) {
            Ok(target) => target,
            Err(error) => {
                let packet = inform_error_packet(protocol, &error.to_string());
                entry.channel.send(&packet).await?;
                entry.channel.update(now_ms()).await?;
                self.log_message(&format!("{peer} -> {error}"));
                return Ok(());
            }
        };
        let target = match resolve_endpoint(&host, port).await {
            Ok(address) => Destination {
                host: host.clone(),
                port,
                address,
            },
            Err(error) => {
                let packet = inform_error_packet(protocol, &error.to_string());
                entry.channel.send(&packet).await?;
                entry.channel.update(now_ms()).await?;
                self.log_message(&format!("{peer} -> 无法解析 {host}:{port}：{error}"));
                return Ok(());
            }
        };

        if protocol == ProtocolType::Udp {
            self.establish_udp(socket, peer, entry, protocol, &target)
                .await
        } else if protocol == ProtocolType::Mux {
            self.establish_mux(socket, peer, entry, protocol, &target)
                .await
        } else {
            self.establish_tcp(socket, peer, entry, protocol, &target)
                .await
        }
    }

    async fn establish_tcp(
        self: &Arc<Self>,
        socket: &UdpEndpoint,
        peer: SocketAddr,
        entry: &HandshakeEntry,
        protocol: ProtocolType,
        target: &Destination,
    ) -> Result<(), ServerError> {
        let Destination {
            host,
            port,
            address,
        } = target;
        let tcp = match TcpSession::connect(*address).await {
            Ok(session) => session,
            Err(error) => {
                let packet = inform_error_packet(protocol, &error.to_string());
                entry.channel.send(&packet).await?;
                entry.channel.update(now_ms()).await?;
                self.log_message(&format!("TCP {host}:{port} 连接失败：{error}"));
                return Ok(());
            }
        };
        tracing::info!("KCP 握手成功：{peer} -> TCP {host}:{port}");

        let (conv, channel) = self.create_data_channel(socket, peer).await?;
        let (reader, writer) = tcp.into_split();
        let session = self.build_session(
            channel.clone(),
            SessionKind::Tcp(Arc::new(Mutex::new(writer))),
            protocol,
        );
        self.sessions.lock().await.insert(conv, session.clone());

        tokio::spawn(tcp_reader_loop(
            reader,
            session.clone(),
            self.counters.clone(),
        ));

        self.send_initialise_response(entry, protocol, conv).await
    }

    async fn establish_udp(
        self: &Arc<Self>,
        socket: &UdpEndpoint,
        peer: SocketAddr,
        entry: &HandshakeEntry,
        protocol: ProtocolType,
        target: &Destination,
    ) -> Result<(), ServerError> {
        let Destination {
            host,
            port,
            address,
        } = target;
        let bind: SocketAddr = if address.is_ipv4() {
            SocketAddr::from(([0, 0, 0, 0], 0))
        } else {
            SocketAddr::from(([0u16; 8], 0))
        };
        let udp = match UdpEndpoint::bind(bind).await {
            Ok(endpoint) => endpoint,
            Err(error) => {
                let packet = inform_error_packet(protocol, &error.to_string());
                entry.channel.send(&packet).await?;
                entry.channel.update(now_ms()).await?;
                self.log_message(&format!("UDP 套接字创建失败：{error}"));
                return Ok(());
            }
        };
        tracing::info!("KCP 握手成功：{peer} -> UDP {host}:{port}");

        let (conv, channel) = self.create_data_channel(socket, peer).await?;
        let session = self.build_session(
            channel.clone(),
            SessionKind::Udp {
                socket: udp.clone(),
                target: *address,
            },
            protocol,
        );
        self.sessions.lock().await.insert(conv, session.clone());

        tokio::spawn(udp_relay_loop(udp, session.clone(), self.counters.clone()));

        self.send_initialise_response(entry, protocol, conv).await
    }

    /// 建立 mux 会话：一条 KCP 通道上复用多条本地连接。
    async fn establish_mux(
        self: &Arc<Self>,
        socket: &UdpEndpoint,
        peer: SocketAddr,
        entry: &HandshakeEntry,
        protocol: ProtocolType,
        target: &Destination,
    ) -> Result<(), ServerError> {
        tracing::info!("KCP 握手成功：{peer} -> mux");
        let (conv, channel) = self.create_data_channel(socket, peer).await?;
        let (tunnel, mut commands, events) = MuxTunnel::new(channel.clone(), conv);

        let session = self.build_session(
            channel,
            SessionKind::Mux {
                tunnel: tunnel.clone(),
            },
            protocol,
        );
        self.sessions.lock().await.insert(conv, session);

        let keep_alive = self.settings.keep_alive;
        let udp_timeout = self.settings.udp_timeout;
        let runner = tunnel.clone();
        tokio::spawn(async move {
            if let Err(error) = runner.run(&mut commands, keep_alive, udp_timeout).await {
                tracing::warn!("mux 通道 conv={conv} 结束：{error}");
            }
            runner.clear_records();
        });

        let state = self.clone();
        let target = target.clone();
        tokio::spawn(async move {
            mux_server_event_loop(state, tunnel, target, events).await;
        });

        self.send_initialise_response(entry, protocol, conv).await
    }

    /// 新建数据通道，返回 `(conv, channel)`
    async fn create_data_channel(
        &self,
        socket: &UdpEndpoint,
        peer: SocketAddr,
    ) -> Result<(u32, Arc<KcpChannel>), ServerError> {
        let conv = self.allocate_conv().await;
        let channel = Arc::new(
            KcpChannel::new_with_fec(
                self.codec.clone(),
                data_config(&self.settings, conv),
                socket.clone(),
                peer,
                self.fec,
            )
            .await?,
        );
        {
            let mut kcp = channel.endpoint.kcp.lock().await;
            kcp.set_rx_minrto(10);
            kcp.update(now_ms());
        }
        Ok((conv, channel))
    }

    fn build_session(
        &self,
        channel: Arc<KcpChannel>,
        kind: SessionKind,
        protocol: ProtocolType,
    ) -> Arc<DataSession> {
        let now = now_secs();
        let keep_alive = i64::from(self.settings.keep_alive);
        Arc::new(DataSession {
            channel,
            kind,
            protocol,
            closed: AtomicBool::new(false),
            expire_at: AtomicI64::new(i64::MAX),
            next_keep_alive: AtomicI64::new(if keep_alive > 0 {
                now + keep_alive
            } else {
                i64::MAX
            }),
            last_send_at: AtomicI64::new(now),
            last_receive_at: AtomicI64::new(now),
        })
    }

    async fn send_initialise_response(
        &self,
        entry: &HandshakeEntry,
        protocol: ProtocolType,
        conv: u32,
    ) -> Result<(), ServerError> {
        let (port_start, port_end) = if is_continuous(&self.settings.listen_ports) {
            (
                *self.settings.listen_ports.first().unwrap_or(&0),
                *self.settings.listen_ports.last().unwrap_or(&0),
            )
        } else {
            (0, 0)
        };
        let response = SettingsWrapper {
            uid: conv,
            port_start,
            port_end,
            outbound_bandwidth: self.settings.outbound_bandwidth,
            inbound_bandwidth: self.settings.inbound_bandwidth,
            user_input_port: 0,
            user_input_ip: String::new(),
        };
        entry
            .channel
            .send(&response_initialise_packet(protocol, &response))
            .await?;
        entry.channel.update(now_ms()).await?;
        Ok(())
    }

    fn target_for(&self, wrapper: &SettingsWrapper) -> Result<(String, u16), ServerError> {
        if self.settings.ignore_destination_address || self.settings.ignore_destination_port {
            if wrapper.user_input_ip.is_empty() || wrapper.user_input_port == 0 {
                return Err(ServerError::Startup("客户端未提供目标地址".to_string()));
            }
            return Ok((wrapper.user_input_ip.clone(), wrapper.user_input_port));
        }

        let host = self
            .settings
            .destination_address_list
            .first()
            .cloned()
            .ok_or_else(|| ServerError::Startup("destination_address is not set".to_string()))?;
        let port = *self
            .settings
            .destination_ports
            .first()
            .ok_or_else(|| ServerError::Startup("destination_port is not set".to_string()))?;
        Ok((host, port))
    }

    async fn allocate_conv(&self) -> u32 {
        loop {
            let conv = rand::random::<u32>();
            if conv >= 32 && conv != u32::MAX && !self.sessions.lock().await.contains_key(&conv) {
                return conv;
            }
        }
    }

    async fn handle_data(
        &self,
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

        session.channel.set_peer(peer);
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
    async fn tick_kcp(&self) {
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
    async fn maintain(&self) {
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
                let packet = create_keep_alive_packet(protocol);
                if session.channel.send(&packet).await.is_ok() {
                    session.channel.mark_keep_alive_sent(now);
                    let _ = session.channel.update(now_ms()).await;
                }
                session.set_next_keep_alive(now + keep_alive);
            }
        }
    }

    /// 状态摘要：每 `LOGGING_GAP_SECONDS` 秒一次
    ///
    /// 与 C++ 一致：只有配置了 `log_status`（即设置了 `log_path`）时才输出。
    async fn maybe_log_status(&self) {
        if self.settings.log_status.as_os_str().is_empty() {
            return;
        }
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
        let receive_inner = to_speed_unit(
            StatusCounters::take(&self.counters.ingress_inner_traffic),
            duration,
        );
        let send_inner = to_speed_unit(
            StatusCounters::take(&self.counters.egress_inner_traffic),
            duration,
        );

        let sessions: Vec<(u32, Arc<DataSession>)> = self
            .sessions
            .lock()
            .await
            .iter()
            .map(|(conv, session)| (*conv, session.clone()))
            .collect();
        let mut send_raw_bytes = 0u64;
        let mut latencies = Vec::new();
        for (conv, session) in &sessions {
            send_raw_bytes += session.channel.take_output_bytes();
            latencies.push((*conv, session.channel.rx_srtt().await));
        }
        {
            let handshakes: Vec<Arc<HandshakeEntry>> =
                self.handshakes.lock().await.values().cloned().collect();
            for entry in handshakes {
                send_raw_bytes += entry.channel.take_output_bytes();
            }
        }
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
        for (conv, srtt) in latencies {
            output.push_str(&format!("KCP#{conv} average latency: {srtt} ms\n"));
        }
        output.push('\n');

        let _ = print_status_to_file(&self.settings.log_status, &output);
        // 对应 C++ `std::cout << output_text << std::endl;`（末尾再补一个换行）
        println!("{output}");
    }

    fn log_message(&self, message: &str) {
        let line = format!("{}{message}\n", time_string_with_brackets());
        tracing::warn!("{}", message);
        let _ = print_message_to_file(&self.settings.log_messages, &line);
    }
}

async fn tcp_reader_loop(
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

async fn udp_relay_loop(
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

/// 服务端 mux 事件循环：把对端数据落到本地目标，并维护 `connection_id` 映射。
async fn mux_server_event_loop(
    state: Arc<SocketState>,
    tunnel: Arc<MuxTunnel>,
    default_target: Destination,
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
                        match create_mux_local(
                            &state,
                            &tunnel,
                            protocol,
                            connection_id,
                            &default_target,
                        )
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
                match resolve_endpoint(&host, port).await {
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
            let bind: SocketAddr = if target.address.is_ipv4() {
                SocketAddr::from(([0, 0, 0, 0], 0))
            } else {
                SocketAddr::from(([0u16; 8], 0))
            };
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

/// FEC 配置：只有 k 与 r 都大于 0 时才启用。
fn fec_pair(settings: &UserSettings) -> Option<(u8, u8)> {
    let original = settings.fec_original_packet_count;
    let redundant = settings.fec_redundant_packet_count;
    (original > 0 && redundant > 0).then_some((original, redundant))
}

/// Windows 上 UDP 对端不可达会以「连接重置/中止」的形式返回（WSAECONNRESET）。
fn is_connection_reset(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
    )
}

fn is_continuous(ports: &[u16]) -> bool {
    if ports.is_empty() {
        return false;
    }
    ports
        .windows(2)
        .all(|pair| pair[1] == pair[0].saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuous_ports() {
        assert!(is_continuous(&[3000]));
        assert!(is_continuous(&[3000, 3001, 3002]));
        assert!(!is_continuous(&[3000, 3002]));
        assert!(!is_continuous(&[]));
    }

    #[test]
    fn timeout_constants_match_cpp() {
        assert_eq!(KEEPALIVE_TIMEOUT, 31);
        assert_eq!(TIME_GAP_SECONDS, 255);
        assert_eq!(KCP_CLEANUP_WAITS, 4);
    }
}
