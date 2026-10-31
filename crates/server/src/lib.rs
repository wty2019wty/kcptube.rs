// SPDX-License-Identifier: GPL-3.0-only

//! 服务端模式（对应 C++ `modes/server.cpp`）。
//!
//! M3 范围：UDP 监听、`initialise` 握手、单通道 TCP `raw_data` 转发。
//! 端口跳跃、STUN、DDNS、FEC、mux 留到后续里程碑。

use kcptube_config::UserSettings;
use kcptube_kcp::{Kcp, KcpConfig, KcpError};
use kcptube_protocol::ProtocolError;
use kcptube_protocol::constant::PACKET_LENGTH;
use kcptube_protocol::packet::{
    Feature, ProtocolType, SettingsWrapper, create_data_packet, create_keep_alive_response_packet,
    inform_disconnect_packet, inform_error_packet, response_initialise_packet, unpack_inner,
};
use kcptube_transport::{
    KcpChannel, PacketCodec, TcpSession, TransportError, UdpEndpoint, now_ms, resolve_endpoint,
};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex;
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
            let socket = UdpEndpoint::bind(addr)
                .await
                .map_err(|error| ServerError::Startup(format!("无法监听 {addr}: {error}")))?;
            tracing::info!("服务端监听 UDP {addr}");

            let state = Arc::new(SocketState {
                settings: settings.clone(),
                codec: codec.clone(),
                handshakes: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
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
}

struct DataSession {
    channel: Arc<KcpChannel>,
    writer: Arc<Mutex<OwnedWriteHalf>>,
    closed: AtomicBool,
}

struct SocketState {
    settings: Arc<UserSettings>,
    codec: Arc<PacketCodec>,
    handshakes: Mutex<HashMap<SocketAddr, Arc<HandshakeEntry>>>,
    sessions: Mutex<HashMap<u32, Arc<DataSession>>>,
}

impl SocketState {
    async fn run(self: Arc<Self>, socket: UdpEndpoint) -> Result<(), ServerError> {
        let mut buffer = vec![0u8; PACKET_LENGTH + 256];
        let mut ticker = interval(Duration::from_millis(5));

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
                        Err(error) => {
                            tracing::warn!("UDP 接收失败：{error}");
                        }
                    }
                }
                _ = ticker.tick() => {
                    self.tick(&socket).await;
                }
            }
        }
    }

    async fn handle_datagram(
        &self,
        socket: &UdpEndpoint,
        peer: SocketAddr,
        datagram: &[u8],
    ) -> Result<(), ServerError> {
        let (_, payload) = self.codec.open(datagram)?;
        if payload.is_empty() {
            return Ok(());
        }

        match Kcp::peek_conv(&payload) {
            Some(0) => self.handle_handshake(socket, peer, &payload).await,
            Some(conv) => self.handle_data(conv, peer, &payload).await,
            None => Ok(()),
        }
    }

    async fn handle_handshake(
        &self,
        socket: &UdpEndpoint,
        peer: SocketAddr,
        payload: &[u8],
    ) -> Result<(), ServerError> {
        let entry = self.handshake_entry(socket, peer).await?;
        entry.channel.input_payload(payload).await?;

        let mut buffer = vec![0u8; PACKET_LENGTH];
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
                    self.establish_tcp(socket, peer, &entry, protocol, settings)
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
            KcpChannel::new(
                self.codec.clone(),
                handshake_config(self.settings.kcp_mtu),
                socket.clone(),
                peer,
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
        });
        self.handshakes.lock().await.insert(peer, entry.clone());
        Ok(entry)
    }

    async fn establish_tcp(
        &self,
        socket: &UdpEndpoint,
        peer: SocketAddr,
        entry: &HandshakeEntry,
        protocol: ProtocolType,
        wrapper: SettingsWrapper,
    ) -> Result<(), ServerError> {
        let (host, port) = self.target_for(&wrapper)?;
        let target = resolve_endpoint(&host, port).await?;

        let tcp = match TcpSession::connect(target).await {
            Ok(session) => session,
            Err(error) => {
                let packet = inform_error_packet(protocol, &error.to_string());
                entry.channel.send(&packet).await?;
                entry.channel.update(now_ms()).await?;
                return Ok(());
            }
        };
        tracing::info!("KCP 握手成功：{peer} -> TCP {host}:{port}");

        let conv = self.allocate_conv().await;
        let channel = Arc::new(
            KcpChannel::new(
                self.codec.clone(),
                data_config(&self.settings, conv),
                socket.clone(),
                peer,
            )
            .await?,
        );
        {
            let mut kcp = channel.endpoint.kcp.lock().await;
            kcp.set_rx_minrto(10);
            kcp.update(now_ms());
        }

        let (reader, writer) = tcp.into_split();
        let session = Arc::new(DataSession {
            channel: channel.clone(),
            writer: Arc::new(Mutex::new(writer)),
            closed: AtomicBool::new(false),
        });
        self.sessions.lock().await.insert(conv, session);

        tokio::spawn(tcp_reader_loop(reader, channel));

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
        payload: &[u8],
    ) -> Result<(), ServerError> {
        let session = {
            let sessions = self.sessions.lock().await;
            sessions.get(&conv).cloned()
        };
        let Some(session) = session else {
            return Ok(());
        };

        session.channel.set_peer(peer);
        session.channel.input_payload(payload).await?;
        tracing::trace!("收到 conv={conv} 的 KCP 负载（{} 字节）", payload.len());
        self.drain_data(&session).await;
        Ok(())
    }

    async fn drain_data(&self, session: &DataSession) {
        let mut buffer = vec![0u8; PACKET_LENGTH];
        loop {
            let size = match session.channel.receive(&mut buffer).await {
                Ok(size) => size,
                Err(TransportError::Kcp(KcpError::WouldBlock)) => break,
                Err(_) => {
                    session.closed.store(true, Ordering::SeqCst);
                    break;
                }
            };

            let Ok((feature, protocol, body)) = unpack_inner(&buffer[..size]) else {
                session.closed.store(true, Ordering::SeqCst);
                break;
            };
            match feature {
                Feature::RawData if protocol == ProtocolType::Tcp => {
                    tracing::trace!("转发 {} 字节到本地 TCP", body.len());
                    let mut writer = session.writer.lock().await;
                    if writer.write_all(body).await.is_err() {
                        session.closed.store(true, Ordering::SeqCst);
                        break;
                    }
                }
                Feature::KeepAlive => {
                    let _ = session
                        .channel
                        .send(&create_keep_alive_response_packet(protocol))
                        .await;
                }
                Feature::KeepAliveResponse => {}
                Feature::Disconnect | Feature::Failure => {
                    session.closed.store(true, Ordering::SeqCst);
                    break;
                }
                _ => {}
            }
        }

        if session.closed.load(Ordering::SeqCst) {
            let _ = session
                .channel
                .send(&inform_disconnect_packet(ProtocolType::Tcp))
                .await;
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, item| !std::ptr::eq(item.as_ref(), session));
        }
    }

    async fn tick(&self, _socket: &UdpEndpoint) {
        let now = now_ms();

        let handshakes: Vec<Arc<HandshakeEntry>> =
            self.handshakes.lock().await.values().cloned().collect();
        for entry in handshakes {
            let _ = entry.channel.update(now).await;
        }

        let sessions: Vec<Arc<DataSession>> =
            self.sessions.lock().await.values().cloned().collect();
        for session in sessions {
            let _ = session.channel.update(now).await;
            self.drain_data(&session).await;
        }
    }
}

async fn tcp_reader_loop(mut reader: tokio::net::tcp::OwnedReadHalf, channel: Arc<KcpChannel>) {
    let mut buffer = vec![0u8; 2048];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(size) => {
                tracing::trace!("TCP 读到 {size} 字节，发送 raw_data");
                let packet = create_data_packet(ProtocolType::Tcp, &buffer[..size]);
                if channel.send(&packet).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = channel
        .send(&inform_disconnect_packet(ProtocolType::Tcp))
        .await;
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
}
