// SPDX-License-Identifier: GPL-3.0-only

//! 服务端运行期状态：握手表、数据会话表与共享 socket 状态。

use crate::util::fec_pair;
use kcptube_config::UserSettings;
use kcptube_logging::{
    LOGGING_GAP_SECONDS, StatusCounters, print_message_to_file, time_string_with_brackets,
};
use kcptube_protocol::packet::ProtocolType;
use kcptube_transport::{KcpChannel, MuxTunnel, PacketCodec, UdpEndpoint, now_secs};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex;

pub(crate) struct HandshakeEntry {
    pub(crate) channel: Arc<KcpChannel>,
    pub(crate) established: AtomicBool,
    /// 建链时刻（Unix 秒），用于回收握手通道
    pub(crate) created_at: i64,
}

/// 数据会话的落地端：TCP 写半边、UDP 目标或 mux 隧道。
pub(crate) enum SessionKind {
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

pub(crate) struct DataSession {
    pub(crate) channel: Arc<KcpChannel>,
    pub(crate) kind: SessionKind,
    pub(crate) protocol: ProtocolType,
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
    pub(crate) fn new(
        channel: Arc<KcpChannel>,
        kind: SessionKind,
        protocol: ProtocolType,
        keep_alive: i64,
    ) -> Self {
        let now = now_secs();
        Self {
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
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// 进入关闭流程：仍保留通道一段时间用于冲刷重传（对应 C++ `expiring_kcp`）。
    pub(crate) fn begin_close(&self, expire_at: i64) {
        self.closed.store(true, Ordering::SeqCst);
        self.expire_at.store(expire_at, Ordering::SeqCst);
    }

    pub(crate) fn expire_at(&self) -> i64 {
        self.expire_at.load(Ordering::SeqCst)
    }

    pub(crate) fn next_keep_alive(&self) -> i64 {
        self.next_keep_alive.load(Ordering::SeqCst)
    }

    pub(crate) fn set_next_keep_alive(&self, at: i64) {
        self.next_keep_alive.store(at, Ordering::SeqCst);
    }

    pub(crate) fn mark_send(&self, at: i64) {
        self.last_send_at.store(at, Ordering::SeqCst);
    }

    pub(crate) fn mark_receive(&self, at: i64) {
        self.last_receive_at.store(at, Ordering::SeqCst);
    }

    pub(crate) fn last_send_at(&self) -> i64 {
        self.last_send_at.load(Ordering::SeqCst)
    }

    pub(crate) fn last_receive_at(&self) -> i64 {
        self.last_receive_at.load(Ordering::SeqCst)
    }
}

/// 解析后的目标地址（用于日志与连接）
#[derive(Clone)]
pub(crate) struct Destination {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) address: SocketAddr,
}

/// 建立数据通道时需要用到的带宽与 RTT 提示。
#[derive(Clone, Copy)]
pub(crate) struct BandwidthHint {
    pub(crate) outbound: u64,
    pub(crate) srtt: i32,
}

pub(crate) struct SocketState {
    pub(crate) settings: Arc<UserSettings>,
    pub(crate) codec: Arc<PacketCodec>,
    /// FEC 配置 `(k, r)`；未启用时为 `None`
    pub(crate) fec: Option<(u8, u8)>,
    /// FEC 原始包数量，用于解析帧类型
    pub(crate) fec_original: Option<usize>,
    pub(crate) handshakes: Mutex<HashMap<SocketAddr, Arc<HandshakeEntry>>>,
    pub(crate) sessions: Mutex<HashMap<u32, Arc<DataSession>>>,
    pub(crate) counters: Arc<StatusCounters>,
    /// 下一次输出状态摘要的时刻（Unix 秒）
    pub(crate) next_status_at: AtomicI64,
}

impl SocketState {
    pub(crate) fn new(settings: Arc<UserSettings>, codec: Arc<PacketCodec>) -> Self {
        let fec = fec_pair(&settings);
        let fec_original = fec.map(|(original, _)| original as usize);
        let next_status_at = AtomicI64::new(now_secs() + LOGGING_GAP_SECONDS as i64);
        Self {
            settings,
            codec,
            fec,
            fec_original,
            handshakes: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            counters: Arc::new(StatusCounters::default()),
            next_status_at,
        }
    }

    pub(crate) fn log_message(&self, message: &str) {
        let line = format!("{}{message}\n", time_string_with_brackets());
        tracing::warn!("{}", message);
        let _ = print_message_to_file(&self.settings.log_messages, &line);
    }
}
