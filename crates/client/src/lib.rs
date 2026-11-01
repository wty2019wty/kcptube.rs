// SPDX-License-Identifier: GPL-3.0-only

//! 客户端模式（对应 C++ `modes/client.cpp`）。
//!
//! 已实现：TCP + UDP 监听、`initialise` 握手（超时后换新端口重试）、
//! `raw_data` 与 mux 转发、FEC、端口跳跃、保活与状态日志、
//! `ignore_listen_*` + `[custom_input*]` 自定义输入映射。
//! STUN/DDNS 属于 M6。

use kcptube_config::{IpVersionOnly, UserSettings};
use kcptube_kcp::{KcpConfig, KcpError};
use kcptube_logging::{
    LOGGING_GAP_SECONDS, StatusCounters, print_message_to_file, print_status_to_file,
    time_string_with_brackets, to_speed_unit,
};
use kcptube_protocol::ProtocolError;
use kcptube_protocol::constant::{
    HANDSHAKE_TIMEOUT, KCP_CLEANUP_WAITS, PACKET_LENGTH, RECEIVER_CLEANUP_WAITS,
};
use kcptube_protocol::packet::{
    Feature, ProtocolType, SettingsWrapper, create_data_packet, create_keep_alive_packet,
    create_keep_alive_response_packet, create_test_connection_packet, inform_disconnect_packet,
    mux_tell_server_connect_address, request_initialise_packet,
    request_initialise_packet_with_address, unpack_inner,
};
use kcptube_transport::{
    KcpChannel, MAX_INNER_MESSAGE, MuxEvent, MuxRecord, MuxSink, MuxTcpSink, MuxTunnel,
    PacketCodec, TcpListenerSession, TcpSession, TransportError, UdpBindOptions, UdpEndpoint,
    now_ms, now_secs, resolve_endpoints_filtered,
};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock, mpsc};
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

/// 一条本地监听项。
///
/// `remote` 是 `[custom_input*]` 映射右侧的固定目标地址；常规监听
/// （`listen_on` + `listen_port`）没有固定目标，由服务端配置决定。
struct ListenSpec {
    address: SocketAddr,
    tcp: bool,
    udp: bool,
    remote: Option<Arc<(String, u16)>>,
}

/// 计算需要监听的端点列表。
///
/// 对应 C++ `client_mode::multiple_listening_tcp` / `multiple_listening_udp`：
/// - 未设置 `ignore_listen_address` / `ignore_listen_port` 时，监听
///   `listen_on` 下每个地址的第一个 `listen_port`（TCP + UDP 同端口）；
/// - 设置后不再监听 `listen_on` / `listen_port`，改为逐条监听
///   `[custom_input]`（TCP + UDP）、`[custom_input_tcp]`（仅 TCP）、
///   `[custom_input_udp]`（仅 UDP）中的映射。
fn build_listen_specs(settings: &UserSettings) -> Result<Vec<ListenSpec>, ClientError> {
    let mut specs = Vec::new();

    if settings.ignore_listen_address || settings.ignore_listen_port {
        collect_mapping_specs(
            settings,
            &settings.user_input_mappings,
            true,
            true,
            &mut specs,
        )?;
        collect_mapping_specs(
            settings,
            &settings.user_input_mappings_tcp,
            true,
            false,
            &mut specs,
        )?;
        collect_mapping_specs(
            settings,
            &settings.user_input_mappings_udp,
            false,
            true,
            &mut specs,
        )?;
        return Ok(specs);
    }

    // 客户端只监听第一个端口（与 C++ `listen_ports.front()` 一致）
    let listen_port = *settings
        .listen_ports
        .first()
        .ok_or_else(|| ClientError::Startup("listen_port is not set".to_string()))?;
    let listen_addresses = if settings.listen_on.is_empty() {
        vec![settings.ip_version_only.wildcard_ip()]
    } else {
        settings
            .listen_on
            .iter()
            .map(|address| {
                address
                    .parse::<IpAddr>()
                    .map_err(|_| ClientError::Startup(format!("无效的 listen_on 地址：{address}")))
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    for ip in listen_addresses {
        if let Some(want_ipv4) = settings.ip_version_only.want_ipv4() {
            if ip.is_ipv4() != want_ipv4 {
                return Err(ClientError::Startup(format!(
                    "listen_on 地址 {ip} 与 ipv4_only/ipv6_only 配置冲突"
                )));
            }
        }
        specs.push(ListenSpec {
            address: SocketAddr::new(ip, listen_port),
            tcp: true,
            udp: true,
            remote: None,
        });
    }
    Ok(specs)
}

/// 把一张 `[custom_input*]` 映射表转换成监听项。
fn collect_mapping_specs(
    settings: &UserSettings,
    mappings: &std::collections::BTreeMap<(String, u16), (String, u16)>,
    tcp: bool,
    udp: bool,
    specs: &mut Vec<ListenSpec>,
) -> Result<(), ClientError> {
    for ((local_address, local_port), (remote_address, remote_port)) in mappings {
        let ip = if local_address.is_empty() {
            settings.ip_version_only.wildcard_ip()
        } else {
            local_address.parse::<IpAddr>().map_err(|_| {
                ClientError::Startup(format!("无效的 custom_input 本地地址：{local_address}"))
            })?
        };

        // 与 C++ 一致：只对 `ipv4_only` 过滤 IPv6 映射；
        // `ipv6_only` 下 C++ 不做过滤，绑定失败会在启动时暴露。
        if settings.ip_version_only == IpVersionOnly::Ipv4 && ip.is_ipv6() {
            let line = format!(
                "{}ipv4_only is set, ignoring IPv6 address{ip}\n",
                time_string_with_brackets()
            );
            eprintln!("{}", line.trim_end());
            let _ = print_message_to_file(&settings.log_messages, &line);
            continue;
        }

        specs.push(ListenSpec {
            address: SocketAddr::new(ip, *local_port),
            tcp,
            udp,
            remote: Some(Arc::new((remote_address.clone(), *remote_port))),
        });
    }
    Ok(())
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

        if settings.listen_ports.is_empty()
            && !settings.ignore_listen_address
            && !settings.ignore_listen_port
        {
            return Err(ClientError::Startup("listen_port is not set".to_string()));
        }
        if settings.destination_address_list.is_empty() {
            return Err(ClientError::Startup(
                "destination_address is not set".to_string(),
            ));
        }
        if settings.destination_ports.is_empty() {
            return Err(ClientError::Startup(
                "destination_port is not set".to_string(),
            ));
        }

        // `ignore_listen_address` / `ignore_listen_port` 为真时不再监听
        // `listen_on` + `listen_port`，改为逐条监听 `[custom_input*]` 映射
        // （对应 C++ `multiple_listening_tcp` / `multiple_listening_udp`）。
        let listen_specs = build_listen_specs(&settings)?;
        if listen_specs.is_empty() {
            return Err(ClientError::Startup("没有可用的监听地址".to_string()));
        }

        let state = Arc::new(ClientState {
            settings: settings.clone(),
            codec: codec.clone(),
            fec: fec_pair(&settings),
            counters: Arc::new(StatusCounters::default()),
            channels: Mutex::new(HashMap::new()),
            udp_relays: Mutex::new(HashMap::new()),
            mux: Mutex::new(Vec::new()),
            mux_cursor: AtomicUsize::new(0),
            destination_ports: RwLock::new(settings.destination_ports.clone()),
            next_channel_id: AtomicU64::new(1),
            next_status_at: AtomicI64::new(now_secs() + LOGGING_GAP_SECONDS as i64),
        });

        let mux_mode = settings.mux_tunnels > 0;

        let mut tasks = JoinSet::new();

        // mux 模式：在后台维持 `mux_tunnels` 条共享通道（对应 C++ `establish_mux_channels`）。
        // 建链失败不会让客户端退出，而是持续重试；通道断开后也由该任务补建。
        if mux_mode {
            let mux_state = state.clone();
            let target = settings.mux_tunnels as usize;
            tasks.spawn(async move {
                maintain_mux_tunnels(mux_state, target).await;
                Ok::<(), ClientError>(())
            });
        }

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

        // 与 C++ `udp_server::initialise` 一致：IPv6 监听地址只有
        // `ipv6_only=yes` 时才设置 IPV6_V6ONLY，否则是双栈。
        let v6only = settings.ip_version_only.ipv6_v6only();
        for spec in listen_specs {
            let addr = spec.address;

            if spec.tcp {
                let listener =
                    match TcpListenerSession::bind_with(addr, addr.is_ipv6().then_some(v6only))
                        .await
                    {
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
                let remote = spec.remote.clone();
                if mux_mode {
                    tasks.spawn(async move { mux_accept_loop(listener, tcp_state, remote).await });
                } else {
                    tasks.spawn(async move { accept_loop(listener, tcp_state, remote).await });
                }
            }

            if spec.udp {
                let udp_listener = match UdpEndpoint::bind_with(
                    addr,
                    UdpBindOptions {
                        reuse_address: true,
                        only_v6: addr.is_ipv6().then_some(v6only),
                    },
                )
                .await
                {
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
                let remote = spec.remote.clone();
                if mux_mode {
                    tasks.spawn(async move {
                        mux_udp_listen_loop(udp_listener, udp_state, remote).await
                    });
                } else {
                    tasks.spawn(
                        async move { udp_listen_loop(udp_listener, udp_state, remote).await },
                    );
                }
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
    /// mux 模式下的共享隧道池（`mux_tunnels` 条）
    mux: Mutex<Vec<Arc<MuxTunnel>>>,
    /// mux 通道轮询游标
    mux_cursor: AtomicUsize,
    /// 动态目标端口列表（握手后可能被服务端返回的端口范围覆盖）
    destination_ports: RwLock<Vec<u16>>,
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

    /// 随机挑一个目标地址与目标端口（对应 C++ `randomly_pick_index` +
    /// `generate_new_port_number`）。
    async fn pick_destination(&self) -> Result<(String, u16), ClientError> {
        let host = random_pick(&self.settings.destination_address_list)
            .cloned()
            .ok_or_else(|| ClientError::Startup("destination_address is not set".to_string()))?;
        let port = {
            let ports = self.destination_ports.read().await;
            random_pick(&ports)
                .copied()
                .ok_or_else(|| ClientError::Startup("destination_port is not set".to_string()))?
        };
        Ok((host, port))
    }

    /// 用服务端握手响应里的端口范围覆盖动态目标端口列表。
    ///
    /// 与 C++ `on_handshake_success` 一致：`port_start`/`port_end` 均非 0 时，
    /// 生成 `port_start..port_end`（右开区间）。
    async fn update_destination_ports(&self, port_start: u16, port_end: u16) {
        if port_start == 0 || port_end == 0 || port_end <= port_start {
            return;
        }
        let ports: Vec<u16> = (port_start..port_end).collect();
        if !ports.is_empty() {
            *self.destination_ports.write().await = ports;
        }
    }

    /// 建立一条 mux 隧道并启动收发任务。
    ///
    /// 通道断开后会从 `state.mux` 中移除，由 `maintain_mux_tunnels` 统一补建。
    fn spawn_mux_tunnel(self: &Arc<Self>, conv: u32, channel: Arc<KcpChannel>) -> Arc<MuxTunnel> {
        let (tunnel, mut commands, events) = MuxTunnel::new(channel, conv);
        let keep_alive = self.settings.keep_alive;
        let udp_timeout = self.settings.udp_timeout;

        let state = self.clone();
        let runner = tunnel.clone();
        tokio::spawn(async move {
            let id = state.register_channel(conv, runner.channel().clone()).await;
            if let Err(error) = runner.run(&mut commands, keep_alive, udp_timeout).await {
                tracing::warn!("mux 通道 conv={conv} 结束：{error}");
            }
            runner.clear_records();
            state.unregister_channel(id).await;
            state
                .mux
                .lock()
                .await
                .retain(|item| !Arc::ptr_eq(item, &runner));
        });

        let events_tunnel = tunnel.clone();
        tokio::spawn(async move {
            mux_event_loop(events_tunnel, events).await;
        });

        tunnel
    }

    /// 轮询选取一条 mux 隧道（对应 C++ `pick_one_from_kcp_channels`）。
    async fn pick_mux_tunnel(&self) -> Option<Arc<MuxTunnel>> {
        let tunnels = self.mux.lock().await;
        if tunnels.is_empty() {
            return None;
        }
        let index = self.mux_cursor.fetch_add(1, Ordering::Relaxed) % tunnels.len();
        tunnels.get(index).cloned()
    }

    /// 按本地 UDP 源地址查找已有的 mux 记录。
    async fn find_udp_record(&self, peer: SocketAddr) -> Option<Arc<MuxRecord>> {
        let tunnels = self.mux.lock().await;
        for tunnel in tunnels.iter() {
            if let Some(record) = tunnel.find_udp_record(peer) {
                return Some(record);
            }
        }
        None
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

    let keep_alive = i64::from(state.settings.keep_alive);
    let mut next_keep_alive = now_secs() + keep_alive;
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

/// mux 模式下的 TCP 接入：复用已建立的通道，按 `connection_id` 分流。
async fn mux_accept_loop(
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

/// 分配一个未占用的 `connection_id`（对应 C++ `generate_random_number<uint32_t>`）。
fn allocate_connection_id(tunnel: &MuxTunnel) -> u32 {
    loop {
        let id = rand::random::<u32>();
        if tunnel.record(id).is_none() {
            return id;
        }
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
async fn mux_udp_listen_loop(
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
async fn mux_event_loop(tunnel: Arc<MuxTunnel>, mut events: mpsc::Receiver<MuxEvent>) {
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
async fn maintain_mux_tunnels(state: Arc<ClientState>, target: usize) {
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

/// 建立一条数据通道（含握手与 30 秒超时换端口重试）。
///
/// 与 C++ `create_handshake` + `handshake_timeout_detection` 的行为一致：
/// 超时后丢弃旧 socket，重新解析服务端地址并发起新握手。
///
/// 与 C++ 的差异（更稳健）：域名解析出的**所有**地址会依次尝试。
/// C++ 用双栈 socket + `v4_mapped` 解析，天然覆盖 A/AAAA 两种记录；
/// Rust 的 socket 有明确地址族，因此某个地址不可达时继续尝试下一个，
/// 避免「DDNS 同时有 A/AAAA 记录、服务端只监听 IPv4」时永远连不上。
async fn establish_channel(
    state: &Arc<ClientState>,
    protocol: ProtocolType,
    remote: Option<&(String, u16)>,
) -> Result<(u32, Arc<KcpChannel>), ClientError> {
    loop {
        let (host, port) = state.pick_destination().await?;
        let candidates = match resolve_endpoints_filtered(
            &host,
            port,
            state.settings.ip_version_only.want_ipv4(),
        )
        .await
        {
            Ok(candidates) => candidates,
            Err(error) => {
                state.log_message(&format!("无法解析 {host}:{port}：{error}"));
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        // 多个候选地址共享同一个握手超时预算：只有一个候选时保持 C++ 的
        // 30 秒；有多个时均分（每个至少 5 秒），避免第一个地址不可达时
        // 把整段时间耗在它身上。
        let per_candidate =
            Duration::from_secs((HANDSHAKE_TIMEOUT as u64 / candidates.len() as u64).max(5));

        for server_peer in candidates {
            match try_establish_channel(state, protocol, remote, &host, server_peer, per_candidate)
                .await
            {
                Ok(Some(result)) => return Ok(result),
                // 该候选地址握手失败（超时 / 对端不可达）：换下一个地址
                Ok(None) => continue,
                // 服务端明确返回 failure：本地连接已经没有意义
                Err(error) => return Err(error),
            }
        }
    }
}

/// 对单个已解析的服务端地址尝试一次握手。
///
/// - `Ok(Some(..))`：成功
/// - `Ok(None)`：本次失败（超时、ICMP 不可达等），调用方可换地址/换端口重试
/// - `Err(..)`：服务端明确拒绝（`failure`），不应重试
async fn try_establish_channel(
    state: &Arc<ClientState>,
    protocol: ProtocolType,
    remote: Option<&(String, u16)>,
    host: &str,
    server_peer: SocketAddr,
    timeout: Duration,
) -> Result<Option<(u32, Arc<KcpChannel>)>, ClientError> {
    let socket = match UdpEndpoint::bind(wildcard_addr(server_peer)).await {
        Ok(socket) => socket,
        Err(error) => {
            state.log_message(&format!("无法创建本地 UDP 套接字：{error}"));
            return Ok(None);
        }
    };

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
    handshake.set_blast(state.settings.blast);
    handshake
        .set_bandwidth(
            state.settings.outbound_bandwidth,
            state.settings.inbound_bandwidth,
        )
        .await;
    {
        let mut kcp = handshake.endpoint.kcp.lock().await;
        kcp.set_rx_minrto(10);
    }

    // 对应 C++ `create_handshake`：仅在 `ignore_listen_address` /
    // `ignore_listen_port` 场景（此时一定来自 `[custom_input*]` 映射）
    // 才把用户指定的目标地址随握手包发给服务端。
    let packet = match remote {
        Some((address, port)) => request_initialise_packet_with_address(
            protocol,
            state.settings.outbound_bandwidth,
            state.settings.inbound_bandwidth,
            address,
            *port,
        ),
        None => request_initialise_packet(
            protocol,
            state.settings.outbound_bandwidth,
            state.settings.inbound_bandwidth,
        ),
    };
    // 发送/驱动失败（例如本机没有对应地址族的路由）只算本次尝试失败，
    // 不能直接中断本地连接；交给上层换地址或换端口重试。
    if let Err(error) = handshake.send(&packet).await {
        tracing::warn!("握手包发送失败（{server_peer}）：{error}");
        return Ok(None);
    }
    if let Err(error) = handshake.update(now_ms()).await {
        tracing::warn!("握手通道驱动失败（{server_peer}）：{error}");
        return Ok(None);
    }

    match wait_handshake(state, &handshake, &socket, timeout).await {
        Ok(response) => {
            // 服务端会把自己的监听端口范围回传，客户端据此更新跳跃端口池
            state
                .update_destination_ports(response.port_start, response.port_end)
                .await;

            let conv = response.uid;
            if conv == 0 {
                state.log_message("服务端返回的 conv 为 0，重新握手");
                return Ok(None);
            }
            tracing::debug!("握手完成，conv={conv}（服务端 {host} -> {server_peer}）");

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
            channel.set_blast(state.settings.blast);
            // 对应 C++：客户端出方向带宽受服务端入方向带宽限制
            let outbound = if response.inbound_bandwidth > 0 {
                state
                    .settings
                    .outbound_bandwidth
                    .min(response.inbound_bandwidth)
            } else {
                state.settings.outbound_bandwidth
            };
            channel
                .set_bandwidth(outbound, state.settings.inbound_bandwidth)
                .await;
            channel.reset_window_values(handshake.rx_srtt().await).await;
            {
                let mut kcp = channel.endpoint.kcp.lock().await;
                kcp.set_rx_minrto(10);
                kcp.update(now_ms());
            }

            // 端口跳跃：定时探测新目标并切换发送 socket
            spawn_port_hopper(state, &channel);
            Ok(Some((conv, channel)))
        }
        Err(ClientError::HandshakeTimeout) => {
            state.log_message(&format!(
                "握手超时 {} 秒，切换新的本地端口重试",
                timeout.as_secs()
            ));
            Ok(None)
        }
        Err(ClientError::Startup(message)) => {
            state.log_message(&message);
            Err(ClientError::Startup(message))
        }
        Err(error) => {
            tracing::warn!("握手失败（{server_peer}）：{error}");
            Ok(None)
        }
    }
}

/// 根据对端地址族选择本地通配地址。
fn wildcard_addr(peer: SocketAddr) -> SocketAddr {
    if peer.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    }
}

/// 定时端口跳跃任务（对应 C++ `change_new_port` / `test_before_change`）。
///
/// 每 `dport_refresh` 秒通过一条临时握手通道发送 `test_connection`，
/// 收到响应后用新的目标地址与本地 socket 重绑主通道。
fn spawn_port_hopper(state: &Arc<ClientState>, channel: &Arc<KcpChannel>) {
    let refresh = state.settings.dynamic_port_refresh;
    if refresh <= 0 {
        return;
    }

    let weak = Arc::downgrade(channel);
    let state = state.clone();
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(refresh as u64));
        // `interval` 的第一次 tick 立即完成，先跳过
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let Some(channel) = weak.upgrade() else {
                break;
            };
            match hop_port(&state, &channel).await {
                Ok(Some(old_socket)) => {
                    // 旧 socket 继续收 4 秒，尽量接住切换瞬间在途的报文
                    // （对应 C++ `expiring_forwarders` 在 4 秒后 stop）
                    let reader = channel.clone();
                    tokio::spawn(async move {
                        drain_old_socket(reader, old_socket).await;
                    });
                }
                Ok(None) => {}
                Err(error) => tracing::warn!("端口跳跃失败：{error}"),
            }
        }
    });
}

/// 探测新端口，成功返回旧 socket。
async fn hop_port(
    state: &Arc<ClientState>,
    channel: &Arc<KcpChannel>,
) -> Result<Option<UdpEndpoint>, ClientError> {
    let (host, port) = state.pick_destination().await?;
    let candidates =
        resolve_endpoints_filtered(&host, port, state.settings.ip_version_only.want_ipv4()).await?;

    for peer in candidates {
        if let Some(old) = probe_candidate(state, channel, &host, port, peer).await? {
            return Ok(Some(old));
        }
    }
    Ok(None)
}

/// 用一条临时握手通道向指定地址发送 `test_connection`，成功后重绑主通道。
async fn probe_candidate(
    state: &Arc<ClientState>,
    channel: &Arc<KcpChannel>,
    host: &str,
    port: u16,
    peer: SocketAddr,
) -> Result<Option<UdpEndpoint>, ClientError> {
    let socket = match UdpEndpoint::bind(wildcard_addr(peer)).await {
        Ok(socket) => socket,
        Err(error) => {
            tracing::trace!("端口跳跃无法绑定本地 socket（{peer}）：{error}");
            return Ok(None);
        }
    };

    let probe = Arc::new(
        KcpChannel::new_with_fec(
            state.codec.clone(),
            handshake_config(state.settings.kcp_mtu),
            socket.clone(),
            peer,
            state.fec,
        )
        .await?,
    );
    probe.set_blast(true);
    {
        let mut kcp = probe.endpoint.kcp.lock().await;
        kcp.set_rx_minrto(10);
    }
    probe.send(&create_test_connection_packet()).await?;
    probe.update(now_ms()).await?;

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut ticker = interval(Duration::from_millis(5));
    let mut datagram = vec![0u8; PACKET_LENGTH + 256];
    let mut inner = vec![0u8; MAX_INNER_MESSAGE];

    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::select! {
            _ = ticker.tick() => {
                probe.update(now_ms()).await?;
            }
            result = socket.recv_from(&mut datagram) => {
                if let Ok((size, _)) = result {
                    let _ = probe.process_datagram(&datagram[..size]).await;
                }
            }
            _ = tokio::time::sleep_until(deadline) => return Ok(None),
        }

        loop {
            match probe.receive(&mut inner).await {
                Ok(size) => {
                    let (feature, _, _) = unpack_inner(&inner[..size])?;
                    // `test_connection` 与 `keep_alive` 共用同一 feature 值
                    if matches!(feature, Feature::KeepAlive | Feature::KeepAliveResponse) {
                        let old = channel.rebind(socket.clone(), peer)?;
                        state.log_message(&format!("端口跳跃：切换到 {host}:{port}"));
                        return Ok(Some(old));
                    }
                }
                Err(TransportError::Kcp(KcpError::WouldBlock)) => break,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

/// 切换后继续读取旧 socket 一段时间，避免在途 ACK 被直接丢弃。
async fn drain_old_socket(channel: Arc<KcpChannel>, socket: UdpEndpoint) {
    let deadline = Instant::now() + Duration::from_secs((RECEIVER_CLEANUP_WAITS / 2).max(1) as u64);
    let mut buffer = vec![0u8; PACKET_LENGTH + 256];
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            result = socket.recv_from(&mut buffer) => {
                match result {
                    Ok((size, _)) => {
                        let _ = channel.process_datagram(&buffer[..size]).await;
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

async fn wait_handshake(
    state: &Arc<ClientState>,
    channel: &Arc<KcpChannel>,
    socket: &UdpEndpoint,
    timeout: Duration,
) -> Result<SettingsWrapper, ClientError> {
    let deadline = Instant::now() + timeout;
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
                // 对应 C++ `udp_client::handle_receive`：底层 recv 出错（例如
                // 对端暂时不在线导致的 ICMP 端口不可达 WSAECONNRESET）只是
                // 重新挂起接收，**不会中断握手**；由 30 秒超时统一兜底。
                let Ok((size, _)) = result else {
                    tracing::trace!("握手期间 UDP 接收失败，继续重试");
                    continue;
                };
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

/// 从列表中随机取一个元素（对应 C++ `randomly_pick_index`）。
fn random_pick<T>(items: &[T]) -> Option<&T> {
    if items.is_empty() {
        None
    } else {
        Some(&items[rand::random::<u32>() as usize % items.len()])
    }
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

    fn parse(text: &str) -> UserSettings {
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        UserSettings::parse_lines(&lines).expect("测试配置应合法")
    }

    #[test]
    fn handshake_timeout_matches_cpp() {
        assert_eq!(HANDSHAKE_TIMEOUT, 30);
    }

    #[test]
    fn listen_specs_regular_listen() {
        let settings = parse(
            "mode=client\n\
             listen_on=127.0.0.1\n\
             listen_port=5000\n\
             destination_address=127.0.0.1\n\
             destination_port=6000\n\
             encryption_password=p\n\
             encryption_algorithm=aes-gcm\n",
        );
        let specs = build_listen_specs(&settings).expect("应能生成监听项");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].address, "127.0.0.1:5000".parse().unwrap());
        assert!(specs[0].tcp && specs[0].udp);
        assert!(specs[0].remote.is_none());
    }

    #[test]
    fn listen_specs_from_custom_input_sections() {
        let settings = parse(
            "mode=client\n\
             listen_port={}\n\
             destination_address=127.0.0.1\n\
             destination_port=6000\n\
             encryption_password=p\n\
             encryption_algorithm=aes-gcm\n\
             [custom_input_tcp]\n\
             127.0.0.1:7001->127.0.0.1:8080\n\
             [custom_input_udp]\n\
             127.0.0.1:7002->127.0.0.1:8081\n",
        );
        let specs = build_listen_specs(&settings).expect("应能生成监听项");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].address, "127.0.0.1:7001".parse().unwrap());
        assert!(specs[0].tcp && !specs[0].udp);
        assert_eq!(
            specs[0].remote.as_deref().map(|item| item.0.as_str()),
            Some("127.0.0.1")
        );
        assert_eq!(specs[0].remote.as_ref().map(|item| item.1), Some(8080));
        assert_eq!(specs[1].address, "127.0.0.1:7002".parse().unwrap());
        assert!(!specs[1].tcp && specs[1].udp);
        assert_eq!(specs[1].remote.as_ref().map(|item| item.1), Some(8081));
    }

    #[test]
    fn listen_specs_skip_ipv6_mapping_under_ipv4_only() {
        let settings = parse(
            "mode=client\n\
             listen_port={}\n\
             ipv4_only=yes\n\
             destination_address=127.0.0.1\n\
             destination_port=6000\n\
             encryption_password=p\n\
             encryption_algorithm=aes-gcm\n\
             [custom_input]\n\
             [::1]:7003->127.0.0.1:8080\n\
             127.0.0.1:7004->127.0.0.1:8080\n",
        );
        let specs = build_listen_specs(&settings).expect("应能生成监听项");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].address, "127.0.0.1:7004".parse().unwrap());
    }
}
