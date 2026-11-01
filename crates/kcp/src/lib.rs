// SPDX-License-Identifier: GPL-3.0-only

//! KCP 传输层。
//!
//! 逐行移植 `legacy-cpp/src/3rd_party/ikcp.cpp`（skywind3000/ikcp 的 cnbatch 改版）。
//! 线格式为标准 ikcp（所有整数**小端**），额外保留：
//!
//! - `resendts_buf` / `fastack_buf` 索引，避免每次 flush 全表扫描；
//! - `SetBandwidth` + `ResetWindowValues` 的带宽窗口自适应；
//! - `WaitQueue*` 背压接口；
//! - `nodelay` 支持 0/1/2 三档（C++ 预设 `fast2/fast4/fast6` 使用 2）。
//!
//! 模块划分：`api`（配置与查询）、`send`（发送与 `flush`）、`recv`（接收与重组）、
//! `ack`（ACK / 重传索引）、`timer`（定时驱动）。算法逻辑与拆分前逐行一致。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use thiserror::Error;

// ---- 常量（与 ikcp.cpp 完全一致） ----
const RTO_NDL: u32 = 30;
const RTO_MIN: u32 = 100;
const RTO_DEF: u32 = 200;
const RTO_MAX: u32 = 60_000;
const CMD_PUSH: u8 = 81;
const CMD_ACK: u8 = 82;
const CMD_WASK: u8 = 83;
const CMD_WINS: u8 = 84;
const ASK_SEND: u32 = 1;
const ASK_TELL: u32 = 2;
const WND_SND: u32 = 32;
const WND_RCV: u32 = 128;
const MTU_DEF: u32 = 1400;
const INTERVAL: u32 = 100;
/// ikcp 段头长度
pub const OVERHEAD: u32 = 24;
const DEADLINK: u32 = 20;
const THRESH_INIT: u32 = 2;
const THRESH_MIN: u32 = 2;
const PROBE_INIT: u32 = 7000;
const PROBE_LIMIT: u32 = 120_000;
const FASTACK_LIMIT: i32 = 5;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum KcpError {
    #[error("KCP 未初始化")]
    NotInitialised,
    #[error("发送缓冲区已满")]
    SendQueueFull,
    #[error("接收队列为空（EAGAIN）")]
    WouldBlock,
    #[error("接收缓冲区太小")]
    BufferTooSmall,
    #[error("输入数据非法")]
    InvalidInput,
    #[error("MTU 非法")]
    InvalidMtu,
    #[error("KCP 输出回调失败")]
    OutputFailed,
}

/// KCP 参数（对应 C++ `user_settings.kcp_*` 与命名预设 regular1-5 / fast1-6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KcpConfig {
    pub conv: u32,
    pub mtu: usize,
    /// 0 = 关闭 nodelay，1 = 开启，2 = 更激进（C++ 的 fast2/4/6）
    pub nodelay: i32,
    /// 内部刷新间隔（毫秒）
    pub interval: u32,
    /// 快速重传阈值（0 = 关闭）
    pub resend: u32,
    /// 是否关闭拥塞控制
    pub no_congestion_control: bool,
    pub send_window: u32,
    pub receive_window: u32,
    /// 流模式：把小包合并发送
    pub stream_mode: bool,
}

impl Default for KcpConfig {
    fn default() -> Self {
        Self {
            conv: 0,
            mtu: 1400,
            nodelay: 1,
            interval: 10,
            resend: 2,
            no_congestion_control: true,
            send_window: 1024,
            receive_window: 1024,
            stream_mode: false,
        }
    }
}

impl KcpConfig {
    /// 握手通道的固定参数（对应 C++ `create_handshake` 里的 `kcp_mappings` 构造）。
    ///
    /// 与数据通道不同：`nodelay = 1`、`interval = 1ms`、`resend = 3`、
    /// 窗口固定 32 / 128，不受配置里的 `kcp_*` 影响。
    pub fn handshake(mtu: i32) -> Self {
        Self {
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
}

/// 默认 KCP MTU（对应 C++ 的 `kcp_mtu_with_fec`）
pub const MTU_WITH_FEC_DEFAULT: usize = 1405;

/// KCP 输出回调：底层发送一个 UDP 负载
pub type OutputFn = Box<dyn Fn(&[u8]) -> Result<(), KcpError> + Send + Sync>;
type SharedOutput = Arc<dyn Fn(&[u8]) -> Result<(), KcpError> + Send + Sync>;

/// 一个 KCP 段（对应 C++ `segment`）
#[derive(Debug, Clone, Default)]
struct Segment {
    conv: u32,
    cmd: u8,
    frg: u8,
    wnd: u32,
    ts: u32,
    sn: u32,
    una: u32,
    len: u32,
    resendts: u32,
    rto: u32,
    fastack: u32,
    xmit: u32,
    data: Vec<u8>,
}

impl Segment {
    fn with_size(size: usize) -> Self {
        Self {
            len: size as u32,
            data: vec![0u8; size],
            ..Self::default()
        }
    }
}

#[inline]
fn timediff(later: u32, earlier: u32) -> i32 {
    later.wrapping_sub(earlier) as i32
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// KCP 会话
pub struct Kcp {
    conv: u32,
    mtu: u32,
    mss: u32,
    state: u32,
    snd_una: u32,
    snd_nxt: u32,
    rcv_nxt: u32,
    ssthresh: u32,
    rx_rttval: i32,
    rx_srtt: i32,
    rx_rto: i32,
    rx_minrto: i32,
    snd_wnd: u32,
    rcv_wnd: u32,
    rmt_wnd: u32,
    cwnd: u32,
    probe: u32,
    current: u32,
    interval: u32,
    ts_flush: u32,
    nodelay: u32,
    updated: u32,
    ts_probe: u32,
    probe_wait: u32,
    dead_link: u32,
    incr: u32,
    snd_queue: VecDeque<Segment>,
    rcv_queue: VecDeque<Segment>,
    /// SN -> segment
    snd_buf: BTreeMap<u32, Segment>,
    /// resendts -> {SN}
    resendts_buf: BTreeMap<u32, BTreeSet<u32>>,
    /// fastack -> {SN}
    fastack_buf: BTreeMap<u32, BTreeSet<u32>>,
    /// SN -> segment
    rcv_buf: BTreeMap<u32, Segment>,
    /// (sn, ts)
    acklist: Vec<(u32, u32)>,
    fastresend: i32,
    fastlimit: i32,
    nocwnd: i32,
    stream: i32,

    output: Option<SharedOutput>,
    outbound_bandwidth: u64,
    inbound_bandwidth: u64,
    last_input_time: i64,
    received_data_average_peak: i64,
    sent_data_average_peak: i64,
}

impl std::fmt::Debug for Kcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kcp")
            .field("conv", &self.conv)
            .field("mtu", &self.mtu)
            .field("snd_una", &self.snd_una)
            .field("snd_nxt", &self.snd_nxt)
            .field("rcv_nxt", &self.rcv_nxt)
            .field("snd_queue", &self.snd_queue.len())
            .field("snd_buf", &self.snd_buf.len())
            .field("rcv_queue", &self.rcv_queue.len())
            .field("rcv_buf", &self.rcv_buf.len())
            .finish()
    }
}

// ---- 子模块：`Kcp` 的方法按职责拆分，私有字段通过 `use super::*` 共享 ----
mod ack;
mod api;
mod recv;
mod send;
mod timer;

#[cfg(test)]
mod tests;
