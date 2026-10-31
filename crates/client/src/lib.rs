// SPDX-License-Identifier: GPL-3.0-only

//! 客户端模式（对应 C++ `modes/client.cpp`）。
//!
//! M3 范围：本地 TCP 监听、`initialise` 握手、单通道 `raw_data` 转发。
//! 端口跳跃、mux、STUN、FEC 留到后续里程碑。

use kcptube_config::UserSettings;
use kcptube_kcp::{KcpConfig, KcpError};
use kcptube_protocol::ProtocolError;
use kcptube_protocol::constant::PACKET_LENGTH;
use kcptube_protocol::packet::{
    Feature, ProtocolType, SettingsWrapper, create_data_packet, create_keep_alive_packet,
    create_keep_alive_response_packet, inform_disconnect_packet, request_initialise_packet,
    unpack_inner,
};
use kcptube_transport::{
    KcpChannel, PacketCodec, TcpListenerSession, TcpSession, TransportError, UdpEndpoint, now_ms,
    resolve_endpoint,
};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

        let mut tasks = JoinSet::new();
        for address in listen_addresses {
            let ip: IpAddr = address
                .parse()
                .map_err(|_| ClientError::Startup(format!("无效的 listen_on 地址：{address}")))?;
            for port in &settings.listen_ports {
                let addr = SocketAddr::new(ip, *port);
                let listener = TcpListenerSession::bind(addr).await.map_err(|error| {
                    ClientError::Startup(format!("无法监听 TCP {addr}: {error}"))
                })?;
                tracing::info!("客户端监听 TCP {addr}");

                let settings = settings.clone();
                let codec = codec.clone();
                tasks.spawn(
                    async move { accept_loop(listener, settings, codec, server_peer).await },
                );
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

async fn accept_loop(
    listener: TcpListenerSession,
    settings: Arc<UserSettings>,
    codec: Arc<PacketCodec>,
    server_peer: SocketAddr,
) -> Result<(), ClientError> {
    loop {
        match listener.accept().await {
            Ok((session, peer)) => {
                tracing::debug!("接受本地 TCP 连接 {peer}");
                let settings = settings.clone();
                let codec = codec.clone();
                tokio::spawn(async move {
                    if let Err(error) =
                        handle_connection(settings, codec, server_peer, session).await
                    {
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

async fn handle_connection(
    settings: Arc<UserSettings>,
    codec: Arc<PacketCodec>,
    server_peer: SocketAddr,
    tcp: TcpSession,
) -> Result<(), ClientError> {
    let bind_addr: SocketAddr = if server_peer.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let socket = UdpEndpoint::bind(bind_addr).await?;

    let handshake = Arc::new(
        KcpChannel::new(
            codec.clone(),
            handshake_config(settings.kcp_mtu),
            socket.clone(),
            server_peer,
        )
        .await?,
    );
    {
        let mut kcp = handshake.endpoint.kcp.lock().await;
        kcp.set_rx_minrto(10);
    }

    let packet = request_initialise_packet(
        ProtocolType::Tcp,
        settings.outbound_bandwidth,
        settings.inbound_bandwidth,
    );
    handshake.send(&packet).await?;
    handshake.update(now_ms()).await?;

    let response = wait_handshake(&handshake, &socket).await?;
    let conv = response.uid;
    if conv == 0 {
        return Err(ClientError::Startup("服务端返回的 conv 为 0".to_string()));
    }
    tracing::debug!("握手完成，conv={conv}");

    let channel = Arc::new(
        KcpChannel::new(
            codec,
            data_config(&settings, conv),
            socket.clone(),
            server_peer,
        )
        .await?,
    );
    {
        let mut kcp = channel.endpoint.kcp.lock().await;
        kcp.set_rx_minrto(10);
        kcp.update(now_ms());
    }

    relay_tcp(tcp, channel, socket, &settings).await
}

async fn wait_handshake(
    channel: &Arc<KcpChannel>,
    socket: &UdpEndpoint,
) -> Result<SettingsWrapper, ClientError> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut ticker = interval(Duration::from_millis(5));
    let mut datagram = vec![0u8; PACKET_LENGTH + 256];
    let mut inner_buffer = vec![0u8; PACKET_LENGTH];

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
                                "服务端拒绝连接：{}",
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
    tcp: TcpSession,
    channel: Arc<KcpChannel>,
    socket: UdpEndpoint,
    settings: &UserSettings,
) -> Result<(), ClientError> {
    let (mut reader, mut writer) = tcp.into_split();
    let mut ticker = interval(Duration::from_millis(5));
    let mut tcp_buffer = vec![0u8; 2048];
    let mut datagram = vec![0u8; PACKET_LENGTH + 256];
    let mut inner_buffer = vec![0u8; PACKET_LENGTH];

    let keep_alive = settings.keep_alive;
    let mut next_keep_alive = Instant::now() + Duration::from_secs(u64::from(keep_alive.max(1)));

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
                        channel.send(&packet).await?;
                    }
                }
            }
            result = socket.recv_from(&mut datagram) => {
                if let Ok((size, _)) = result {
                    let _ = channel.process_datagram(&datagram[..size]).await;
                }
            }
            _ = ticker.tick() => {
                if keep_alive > 0 && Instant::now() >= next_keep_alive {
                    let packet = create_keep_alive_packet(ProtocolType::Tcp);
                    channel.send(&packet).await?;
                    next_keep_alive = Instant::now()
                        + Duration::from_secs(u64::from(keep_alive));
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
                Feature::KeepAliveResponse => {}
                Feature::Disconnect | Feature::Failure => break 'relay,
                _ => {}
            }
        }
    }

    Ok(())
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
