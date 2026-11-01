// SPDX-License-Identifier: GPL-3.0-only

//! relay 运行期状态：握手表、数据会话表、两段统计与两侧编解码器。

use crate::RelayError;
use kcptube_config::UserSettings;
use kcptube_logging::{LOGGING_GAP_SECONDS, StatusCounters, log_warning};
use kcptube_protocol::packet::ProtocolType;
use kcptube_transport::{KcpChannel, PacketCodec, UdpEndpoint, now_secs};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use tokio::sync::{Mutex, mpsc};

/// relay 的全局状态，由全部监听 socket 共享。
pub(crate) struct RelayState {
    /// 根配置：日志路径、`config_filename`
    pub(crate) settings: Arc<UserSettings>,
    pub(crate) ingress: Arc<UserSettings>,
    pub(crate) egress: Arc<UserSettings>,
    pub(crate) ingress_codec: Arc<PacketCodec>,
    pub(crate) egress_codec: Arc<PacketCodec>,
    pub(crate) ingress_fec: Option<(u8, u8)>,
    pub(crate) egress_fec: Option<(u8, u8)>,
    pub(crate) ingress_fec_original: Option<usize>,
    /// 客户端源地址 → 握手条目（对应 C++ `handshake_ingress_map_to_channels`）
    pub(crate) handshakes: Mutex<HashMap<SocketAddr, Arc<HandshakeEntry>>>,
    /// `conv` → 数据会话（对应 C++ `id_map_to_both_sides`）
    pub(crate) sessions: Mutex<HashMap<u32, Arc<RelaySession>>>,
    /// `[Client <-> This]` 段的计数（对应 C++ `listener_status_counters`）
    pub(crate) listener_counters: Arc<StatusCounters>,
    /// `[This <-> Remote]` 段的计数（对应 C++ `forwarder_status_counters`）
    pub(crate) forwarder_counters: Arc<StatusCounters>,
    /// 下一次输出状态摘要的时刻（Unix 秒）
    pub(crate) next_status_at: AtomicI64,
}

impl RelayState {
    pub(crate) fn new(settings: Arc<UserSettings>) -> Result<Self, RelayError> {
        let ingress = settings
            .ingress
            .as_deref()
            .cloned()
            .ok_or_else(|| RelayError::Startup("缺少 [listener] 段".to_string()))?;
        let egress = settings
            .egress
            .as_deref()
            .cloned()
            .ok_or_else(|| RelayError::Startup("缺少 [forwarder] 段".to_string()))?;

        let ingress_codec = Arc::new(PacketCodec::new(
            ingress.encryption,
            &ingress.encryption_password,
        ));
        let egress_codec = Arc::new(PacketCodec::new(
            egress.encryption,
            &egress.encryption_password,
        ));
        let ingress_fec = ingress.fec_parameters();
        let egress_fec = egress.fec_parameters();

        Ok(Self {
            settings,
            ingress: Arc::new(ingress),
            egress: Arc::new(egress),
            ingress_codec,
            egress_codec,
            ingress_fec,
            egress_fec,
            ingress_fec_original: ingress_fec.map(|(k, _)| k as usize),
            handshakes: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            listener_counters: Arc::new(StatusCounters::default()),
            forwarder_counters: Arc::new(StatusCounters::default()),
            next_status_at: AtomicI64::new(now_secs() + LOGGING_GAP_SECONDS as i64),
        })
    }

    pub(crate) async fn session(&self, conv: u32) -> Option<Arc<RelaySession>> {
        self.sessions.lock().await.get(&conv).cloned()
    }

    pub(crate) fn log_message(&self, message: &str) {
        log_warning(&self.settings.log_messages, message);
    }
}

/// 一条客户端的握手条目（对应 C++ `kcp_mappings` 在 conv 0 阶段的状态）。
pub(crate) struct HandshakeEntry {
    pub(crate) peer: SocketAddr,
    /// 面向客户端，共享 ingress 监听 socket
    pub(crate) ingress: Arc<KcpChannel>,
    /// 面向服务端，独占 socket；收到 `initialise` 后才建立
    pub(crate) egress: Mutex<Option<Arc<KcpChannel>>>,
    pub(crate) egress_socket: Mutex<Option<UdpEndpoint>>,
    /// 当前选定的服务端地址
    pub(crate) target: Mutex<Option<SocketAddr>>,
    /// 客户端声明的协议类型（保活包要带上）
    pub(crate) protocol: Mutex<ProtocolType>,
    /// 是否已建立数据会话（之后迟到的 `initialise` 只转发）
    pub(crate) established: AtomicBool,
    pub(crate) created_at: i64,
    /// listener 投递解密后的明文帧
    pub(crate) tx: mpsc::Sender<Vec<u8>>,
}

impl HandshakeEntry {
    pub(crate) fn new(
        peer: SocketAddr,
        ingress: Arc<KcpChannel>,
        tx: mpsc::Sender<Vec<u8>>,
    ) -> Self {
        Self {
            peer,
            ingress,
            egress: Mutex::new(None),
            egress_socket: Mutex::new(None),
            target: Mutex::new(None),
            protocol: Mutex::new(ProtocolType::NotCare),
            established: AtomicBool::new(false),
            created_at: now_secs(),
            tx,
        }
    }

    pub(crate) fn is_established(&self) -> bool {
        self.established.load(Ordering::SeqCst)
    }
}

/// 两侧已建立的数据会话（对应 C++ 的 `kcp_mappings` 数据阶段）。
pub(crate) struct RelaySession {
    pub(crate) conv: u32,
    pub(crate) protocol: ProtocolType,
    /// 面向客户端
    pub(crate) ingress: Arc<KcpChannel>,
    /// 面向真实服务端
    pub(crate) egress: Arc<KcpChannel>,
    /// 与 `egress` 共用的独占 socket（会话任务负责接收）
    pub(crate) egress_socket: UdpEndpoint,
    closed: AtomicBool,
    /// 最近一次向前转发数据帧的时刻（对应 C++ `last_data_transfer_time`）
    last_data_transfer: AtomicI64,
    /// 两侧保活的下一次发送时刻（Unix 秒）
    ingress_next_keep_alive: AtomicI64,
    egress_next_keep_alive: AtomicI64,
    /// listener 投递解密后的明文帧
    pub(crate) tx: mpsc::Sender<Vec<u8>>,
}

impl RelaySession {
    pub(crate) fn new(
        conv: u32,
        protocol: ProtocolType,
        ingress: Arc<KcpChannel>,
        egress: Arc<KcpChannel>,
        egress_socket: UdpEndpoint,
        tx: mpsc::Sender<Vec<u8>>,
    ) -> Self {
        let now = now_secs();
        Self {
            conv,
            protocol,
            ingress,
            egress,
            egress_socket,
            closed: AtomicBool::new(false),
            last_data_transfer: AtomicI64::new(now),
            ingress_next_keep_alive: AtomicI64::new(i64::MAX),
            egress_next_keep_alive: AtomicI64::new(i64::MAX),
            tx,
        }
    }

    /// 标记会话结束（维护任务判定空闲，或任一方向收到 `disconnect` / `failure`）。
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// 记录一次数据转发（心跳与空闲判定用）。
    pub(crate) fn mark_data_transfer(&self, at: i64) {
        self.last_data_transfer.store(at, Ordering::Relaxed);
    }

    pub(crate) fn last_data_transfer(&self) -> i64 {
        self.last_data_transfer.load(Ordering::Relaxed)
    }

    pub(crate) fn ingress_next_keep_alive(&self) -> i64 {
        self.ingress_next_keep_alive.load(Ordering::Relaxed)
    }

    pub(crate) fn set_ingress_next_keep_alive(&self, at: i64) {
        self.ingress_next_keep_alive.store(at, Ordering::Relaxed);
    }

    pub(crate) fn egress_next_keep_alive(&self) -> i64 {
        self.egress_next_keep_alive.load(Ordering::Relaxed)
    }

    pub(crate) fn set_egress_next_keep_alive(&self, at: i64) {
        self.egress_next_keep_alive.store(at, Ordering::Relaxed);
    }

    /// 客户端端口跳跃后更新回包目标（对应 C++ 每次收包更新 `ingress_source_endpoint`）。
    pub(crate) fn update_ingress_peer(&self, peer: SocketAddr) {
        if self.ingress.peer() != peer {
            self.ingress.set_peer(peer);
        }
    }
}
