// SPDX-License-Identifier: GPL-3.0-only

//! 客户端运行期共享状态（对应 C++ `client_mode` 的成员数据）。

use crate::ClientError;
use crate::mux::mux_event_loop;
use crate::util::{fec_pair, random_pick};
use kcptube_config::UserSettings;
use kcptube_logging::{
    LOGGING_GAP_SECONDS, StatusCounters, print_message_to_file, print_status_to_file,
    time_string_with_brackets, to_speed_unit,
};
use kcptube_transport::{KcpChannel, MuxRecord, MuxTunnel, PacketCodec, now_secs};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use tokio::sync::{Mutex, RwLock, mpsc};

pub(crate) struct ClientState {
    pub(crate) settings: Arc<UserSettings>,
    pub(crate) codec: Arc<PacketCodec>,
    /// FEC 配置 `(k, r)`；未启用时为 `None`
    pub(crate) fec: Option<(u8, u8)>,
    pub(crate) counters: Arc<StatusCounters>,
    /// 活跃的数据通道（conv + 通道，仅用于状态日志）
    channels: Mutex<HashMap<u64, (u32, Arc<KcpChannel>)>>,
    /// 本地 UDP 源地址 → 该地址对应中继的入队通道
    pub(crate) udp_relays: Mutex<HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>>,
    /// mux 模式下的共享隧道池（`mux_tunnels` 条）
    pub(crate) mux: Mutex<Vec<Arc<MuxTunnel>>>,
    /// mux 通道轮询游标
    mux_cursor: AtomicUsize,
    /// 动态目标端口列表（握手后可能被服务端返回的端口范围覆盖）
    destination_ports: RwLock<Vec<u16>>,
    next_channel_id: AtomicU64,
    next_status_at: AtomicI64,
}

impl ClientState {
    pub(crate) fn new(settings: Arc<UserSettings>, codec: Arc<PacketCodec>) -> Self {
        let fec = fec_pair(&settings);
        let destination_ports = RwLock::new(settings.destination_ports.clone());
        let next_status_at = AtomicI64::new(now_secs() + LOGGING_GAP_SECONDS as i64);
        Self {
            settings,
            codec,
            fec,
            counters: Arc::new(StatusCounters::default()),
            channels: Mutex::new(HashMap::new()),
            udp_relays: Mutex::new(HashMap::new()),
            mux: Mutex::new(Vec::new()),
            mux_cursor: AtomicUsize::new(0),
            destination_ports,
            next_channel_id: AtomicU64::new(1),
            next_status_at,
        }
    }

    /// 登记一条数据通道，返回的 id 用于结束时注销
    pub(crate) async fn register_channel(&self, conv: u32, channel: Arc<KcpChannel>) -> u64 {
        let id = self.next_channel_id.fetch_add(1, Ordering::Relaxed);
        self.channels.lock().await.insert(id, (conv, channel));
        id
    }

    pub(crate) async fn unregister_channel(&self, id: u64) {
        self.channels.lock().await.remove(&id);
    }

    pub(crate) fn log_message(&self, message: &str) {
        let line = format!("{}{message}\n", time_string_with_brackets());
        tracing::warn!("{}", message);
        let _ = print_message_to_file(&self.settings.log_messages, &line);
    }

    /// 随机挑一个目标地址与目标端口（对应 C++ `randomly_pick_index` +
    /// `generate_new_port_number`）。
    pub(crate) async fn pick_destination(&self) -> Result<(String, u16), ClientError> {
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
    pub(crate) async fn update_destination_ports(&self, port_start: u16, port_end: u16) {
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
    pub(crate) fn spawn_mux_tunnel(
        self: &Arc<Self>,
        conv: u32,
        channel: Arc<KcpChannel>,
    ) -> Arc<MuxTunnel> {
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
    pub(crate) async fn pick_mux_tunnel(&self) -> Option<Arc<MuxTunnel>> {
        let tunnels = self.mux.lock().await;
        if tunnels.is_empty() {
            return None;
        }
        let index = self.mux_cursor.fetch_add(1, Ordering::Relaxed) % tunnels.len();
        tunnels.get(index).cloned()
    }

    /// 按本地 UDP 源地址查找已有的 mux 记录。
    pub(crate) async fn find_udp_record(&self, peer: SocketAddr) -> Option<Arc<MuxRecord>> {
        let tunnels = self.mux.lock().await;
        for tunnel in tunnels.iter() {
            if let Some(record) = tunnel.find_udp_record(peer) {
                return Some(record);
            }
        }
        None
    }

    pub(crate) async fn maybe_log_status(&self) {
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
