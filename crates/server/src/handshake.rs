// SPDX-License-Identifier: GPL-3.0-only

//! 握手处理与数据会话建立（TCP / UDP / mux）。

use crate::ServerError;
use crate::mux::mux_server_event_loop;
use crate::relay::{tcp_reader_loop, udp_relay_loop};
use crate::state::{
    BandwidthHint, DataSession, Destination, HandshakeEntry, SessionKind, SocketState,
};
use crate::util::{data_config, handshake_config, is_continuous};
use kcptube_kcp::KcpError;
use kcptube_protocol::packet::{
    Feature, ProtocolType, SettingsWrapper, create_test_connection_packet, inform_error_packet,
    response_initialise_packet, unpack_inner,
};
use kcptube_transport::{
    IncomingFrame, KcpChannel, MAX_INNER_MESSAGE, MuxTunnel, TcpSession, TransportError,
    UdpEndpoint, now_ms, now_secs, resolve_endpoint_filtered,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

impl SocketState {
    pub(crate) async fn handle_handshake(
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
                    // 握手通道上的 feature 3 在 C++ 里按 `test_connection` 处理，
                    // 服务端应原样回一个 test_connection（对应 `server.cpp` 的同名分支）；
                    // 回 `keep_alive_response` 会让 C++ 客户端的端口探测无法确认成功。
                    entry.channel.send(&create_test_connection_packet()).await?;
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
        channel.set_blast(self.settings.blast);
        channel
            .set_bandwidth(
                self.settings.outbound_bandwidth,
                self.settings.inbound_bandwidth,
            )
            .await;
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
        // 对应 C++ `server.cpp` 的 `switch (prtcl)`：mux 会话在握手阶段
        // **不**解析目标地址（`connect_success = true`），真正的目标由后续
        // `pre_connect_custom_address` 或 `mux_transfer` 指定。因此
        // `ignore_destination_*` 下 mux 隧道仍可建立。
        if protocol == ProtocolType::Mux {
            let outbound = if wrapper.inbound_bandwidth > 0 {
                self.settings
                    .outbound_bandwidth
                    .min(wrapper.inbound_bandwidth)
            } else {
                self.settings.outbound_bandwidth
            };
            let hint = BandwidthHint {
                outbound,
                srtt: entry.channel.rx_srtt().await,
            };
            let target = self.default_destination().await;
            return self
                .establish_mux(socket, peer, entry, protocol, target, hint)
                .await;
        }

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
        let target =
            match resolve_endpoint_filtered(&host, port, self.settings.ip_version_only.want_ipv4())
                .await
            {
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

        // 对应 C++：服务端出方向带宽受客户端入方向带宽限制
        let outbound = if wrapper.inbound_bandwidth > 0 {
            self.settings
                .outbound_bandwidth
                .min(wrapper.inbound_bandwidth)
        } else {
            self.settings.outbound_bandwidth
        };
        let hint = BandwidthHint {
            outbound,
            srtt: entry.channel.rx_srtt().await,
        };

        if protocol == ProtocolType::Udp {
            self.establish_udp(socket, peer, entry, protocol, &target, hint)
                .await
        } else {
            self.establish_tcp(socket, peer, entry, protocol, &target, hint)
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
        hint: BandwidthHint,
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

        let (conv, channel) = self.create_data_channel(socket, peer, hint).await?;
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
        hint: BandwidthHint,
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

        let (conv, channel) = self.create_data_channel(socket, peer, hint).await?;
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
        target: Option<Destination>,
        hint: BandwidthHint,
    ) -> Result<(), ServerError> {
        tracing::info!("KCP 握手成功：{peer} -> mux");
        let (conv, channel) = self.create_data_channel(socket, peer, hint).await?;
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
        hint: BandwidthHint,
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
        channel.set_blast(self.settings.blast);
        channel
            .set_bandwidth(hint.outbound, self.settings.inbound_bandwidth)
            .await;
        channel.reset_window_values(hint.srtt).await;
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
        let keep_alive = i64::from(self.settings.keep_alive);
        Arc::new(DataSession::new(channel, kind, protocol, keep_alive))
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

    /// 配置里的默认目标。
    ///
    /// mux 会话在客户端没有先发 `pre_connect_custom_address` 时使用。
    /// 对应 C++ `create_mux_data_tcp_connection` / `create_mux_data_udp_connection`：
    /// `ignore_destination_*` 下它们只接受客户端指定的地址，没有默认目标，
    /// 因此这里返回 `None`，上层会回 `mux_cancel`。
    async fn default_destination(&self) -> Option<Destination> {
        if self.settings.ignore_destination_address || self.settings.ignore_destination_port {
            return None;
        }
        let host = self.settings.destination_address_list.first()?.clone();
        let port = *self.settings.destination_ports.first()?;
        let address =
            resolve_endpoint_filtered(&host, port, self.settings.ip_version_only.want_ipv4())
                .await
                .ok()?;
        Some(Destination {
            host,
            port,
            address,
        })
    }

    async fn allocate_conv(&self) -> u32 {
        loop {
            let conv = rand::random::<u32>();
            if conv >= 32 && conv != u32::MAX && !self.sessions.lock().await.contains_key(&conv) {
                return conv;
            }
        }
    }
}
