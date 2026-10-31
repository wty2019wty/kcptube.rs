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

impl Kcp {
    pub fn new(config: KcpConfig) -> Self {
        let mut kcp = Self::blank(config.conv);
        let _ = kcp.set_mtu(config.mtu as i32);
        kcp.set_window_size(config.send_window as i32, config.receive_window as i32);
        kcp.set_nodelay(
            config.nodelay,
            config.interval as i32,
            config.resend as i32,
            i32::from(config.no_congestion_control),
        );
        kcp.set_stream_mode(config.stream_mode);
        kcp
    }

    /// 初始化默认字段（对应 `kcp_core::initialise`）
    fn blank(conv: u32) -> Self {
        Self {
            conv,
            mtu: MTU_DEF,
            mss: MTU_DEF - OVERHEAD,
            state: 0,
            snd_una: 0,
            snd_nxt: 0,
            rcv_nxt: 0,
            ssthresh: THRESH_INIT,
            rx_rttval: 0,
            rx_srtt: 0,
            rx_rto: RTO_DEF as i32,
            rx_minrto: RTO_MIN as i32,
            snd_wnd: WND_SND,
            rcv_wnd: WND_RCV,
            rmt_wnd: WND_RCV,
            cwnd: 0,
            probe: 0,
            current: 0,
            interval: INTERVAL,
            ts_flush: INTERVAL,
            nodelay: 0,
            updated: 0,
            ts_probe: 0,
            probe_wait: 0,
            dead_link: DEADLINK,
            incr: 0,
            snd_queue: VecDeque::new(),
            rcv_queue: VecDeque::new(),
            snd_buf: BTreeMap::new(),
            resendts_buf: BTreeMap::new(),
            fastack_buf: BTreeMap::new(),
            rcv_buf: BTreeMap::new(),
            acklist: Vec::new(),
            fastresend: 0,
            fastlimit: FASTACK_LIMIT,
            nocwnd: 0,
            stream: 0,
            output: None,
            outbound_bandwidth: 0,
            inbound_bandwidth: 0,
            last_input_time: now_seconds(),
            received_data_average_peak: 0,
            sent_data_average_peak: 0,
        }
    }

    pub fn conv(&self) -> u32 {
        self.conv
    }

    /// 从原始 UDP 负载读取 conv（对应 `kcp_core::get_conv`）
    pub fn peek_conv(data: &[u8]) -> Option<u32> {
        data.get(..4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub fn set_output(&mut self, output: OutputFn) {
        self.output = Some(Arc::from(output));
    }

    pub fn mtu(&self) -> u32 {
        self.mtu
    }

    pub fn mss(&self) -> u32 {
        self.mss
    }

    pub fn set_mtu(&mut self, mtu: i32) -> Result<(), KcpError> {
        if mtu < 50 || (mtu as u32) < OVERHEAD {
            return Err(KcpError::InvalidMtu);
        }
        self.mtu = mtu as u32;
        self.mss = self.mtu - OVERHEAD;
        Ok(())
    }

    pub fn set_interval(&mut self, interval: i32) {
        self.interval = interval.clamp(1, 5000) as u32;
    }

    /// 对应 `set_nodelay`
    pub fn set_nodelay(&mut self, nodelay: i32, interval: i32, resend: i32, nc: i32) {
        if nodelay >= 0 {
            self.nodelay = nodelay as u32;
            self.rx_minrto = if nodelay != 0 {
                RTO_NDL as i32
            } else {
                RTO_MIN as i32
            };
        }
        if interval >= 0 {
            self.set_interval(interval);
        }
        if resend >= 0 {
            self.fastresend = resend;
        }
        if nc >= 0 {
            self.nocwnd = nc;
        }
    }

    /// 对应 `set_wndsize`
    pub fn set_window_size(&mut self, sndwnd: i32, rcvwnd: i32) {
        if sndwnd > 0 {
            self.snd_wnd = sndwnd as u32;
        }
        if rcvwnd > 0 {
            self.rcv_wnd = (rcvwnd as u32).max(WND_RCV);
        }
    }

    pub fn window_sizes(&self) -> (u32, u32) {
        (self.snd_wnd, self.rcv_wnd)
    }

    pub fn send_window_size(&self) -> u32 {
        self.snd_wnd
    }

    pub fn receive_window_size(&self) -> u32 {
        self.rcv_wnd
    }

    pub fn set_stream_mode(&mut self, enable: bool) {
        self.stream = i32::from(enable);
    }

    pub fn set_bandwidth(&mut self, outbound: u64, inbound: u64) {
        self.outbound_bandwidth = outbound;
        self.inbound_bandwidth = inbound;
    }

    /// 对应 `ResetWindowValues`
    pub fn reset_window_values(&mut self, srtt: i32) {
        if self.outbound_bandwidth == 0 && self.inbound_bandwidth == 0 {
            return;
        }
        let max_srtt = self.rx_srtt.max(srtt);
        let min_srtt = self.rx_srtt.min(srtt);
        let srtt = if min_srtt <= 0 { max_srtt } else { min_srtt };
        if srtt <= 0 {
            return;
        }

        if self.outbound_bandwidth > 0 {
            // 与 C++ 的整数运算顺序一致：bw / mtu * srtt / 1000 * 1.2
            let value = self.outbound_bandwidth / u64::from(self.mtu) * srtt as u64 / 1000;
            self.snd_wnd = ((value as f64 * 1.2) as u32).max(32);
        }
        if self.inbound_bandwidth > 0 {
            let value = self.inbound_bandwidth / u64::from(self.mtu) * srtt as u64 / 1000;
            self.rcv_wnd = ((value as f64 * 1.2) as u32).max(32);
        }
    }

    pub fn rx_srtt(&self) -> i32 {
        self.rx_srtt
    }

    pub fn rx_minrto(&self) -> i32 {
        self.rx_minrto
    }

    pub fn set_rx_minrto(&mut self, value: i32) {
        self.rx_minrto = value;
    }

    pub fn waiting_for_send(&self) -> usize {
        self.snd_buf.len() + self.snd_queue.len()
    }

    pub fn wait_queue_is_full(&self) -> bool {
        self.snd_queue.len() >= self.snd_wnd as usize
    }

    pub fn wait_queue_is_empty(&self) -> bool {
        self.snd_queue.is_empty()
    }

    pub fn wait_queue_above_half_capacity(&self) -> bool {
        self.snd_queue.len() > self.snd_wnd as usize / 2
    }

    pub fn wait_queue_below_half_capacity(&self) -> bool {
        self.snd_queue.len() < self.snd_wnd as usize / 2
    }

    pub fn wait_queue_available_capacity(&self) -> i64 {
        i64::from(self.snd_wnd) - self.snd_queue.len() as i64
    }

    pub fn last_input_time(&self) -> i64 {
        self.last_input_time
    }

    pub fn received_data_average_peak(&self) -> i64 {
        self.received_data_average_peak
    }

    pub fn sent_data_average_peak(&self) -> i64 {
        self.sent_data_average_peak
    }

    /// 连接是否已被判定为 dead link
    pub fn is_dead(&self) -> bool {
        self.state == u32::MAX
    }

    /// 上层发送数据（对应 `ikcp_send`）
    pub fn send(&mut self, data: &[u8]) -> Result<(), KcpError> {
        let mut buffer = data;
        let mut sent = 0usize;

        // 流模式：先尝试追加到最后一个段
        if self.stream != 0 {
            if let Some(seg) = self.snd_queue.back_mut() {
                if seg.len < self.mss {
                    let capacity = (self.mss - seg.len) as usize;
                    let extend = buffer.len().min(capacity);
                    let old_size = seg.data.len();
                    seg.data.resize(old_size + extend, 0);
                    seg.data[old_size..old_size + extend].copy_from_slice(&buffer[..extend]);
                    seg.len = (old_size + extend) as u32;
                    seg.frg = 0;
                    buffer = &buffer[extend..];
                    sent = extend;
                }
            }
            if buffer.is_empty() {
                return Ok(());
            }
        }

        let count = if buffer.len() <= self.mss as usize {
            1
        } else {
            buffer.len().div_ceil(self.mss as usize)
        };
        if count >= WND_RCV as usize {
            return if self.stream != 0 && sent > 0 {
                Ok(())
            } else {
                Err(KcpError::SendQueueFull)
            };
        }

        let mut remaining = buffer;
        for index in 0..count {
            let size = remaining.len().min(self.mss as usize);
            let mut seg = Segment::with_size(size);
            seg.data.copy_from_slice(&remaining[..size]);
            seg.frg = if self.stream == 0 {
                (count - index - 1) as u8
            } else {
                0
            };
            self.snd_queue.push_back(seg);
            remaining = &remaining[size..];
        }
        Ok(())
    }

    /// 上层接收数据（对应 `ikcp_recv`）
    pub fn receive(&mut self, buffer: &mut [u8]) -> Result<usize, KcpError> {
        if self.rcv_queue.is_empty() {
            return Err(KcpError::WouldBlock);
        }

        let peeksize = self.peek_size();
        if peeksize < 0 {
            return Err(KcpError::WouldBlock);
        }
        let peeksize = peeksize as usize;
        if peeksize > buffer.len() {
            return Err(KcpError::BufferTooSmall);
        }

        let recover = self.rcv_queue.len() >= self.rcv_wnd as usize;

        let mut copied = 0usize;
        while let Some(seg) = self.rcv_queue.pop_front() {
            buffer[copied..copied + seg.len as usize].copy_from_slice(&seg.data);
            copied += seg.len as usize;
            if seg.frg == 0 {
                break;
            }
        }

        // 把 rcv_buf 里连续的段搬进 rcv_queue
        while let Some((&sn, _)) = self.rcv_buf.first_key_value() {
            if sn == self.rcv_nxt && self.rcv_queue.len() < self.rcv_wnd as usize {
                if let Some(seg) = self.rcv_buf.remove(&sn) {
                    self.rcv_queue.push_back(seg);
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                }
            } else {
                break;
            }
        }

        if self.rcv_queue.len() < self.rcv_wnd as usize && recover {
            self.probe |= ASK_TELL;
        }

        Ok(copied)
    }

    /// 对应 `peek_size`；返回 -1 表示数据不完整
    pub fn peek_size(&self) -> i32 {
        let Some(seg) = self.rcv_queue.front() else {
            return -1;
        };
        if seg.frg == 0 {
            return seg.len as i32;
        }
        if self.rcv_queue.len() < seg.frg as usize + 1 {
            return -1;
        }

        let mut length = 0i32;
        for seg in &self.rcv_queue {
            length += seg.len as i32;
            if seg.frg == 0 {
                break;
            }
        }
        length
    }

    /// 收到底层数据包（对应 `ikcp_input`）
    pub fn input(&mut self, data: &[u8]) -> Result<(), KcpError> {
        let result = self.input_inner(data);
        self.last_input_time = now_seconds();
        if result.is_ok() {
            self.received_data_average_peak =
                (7 * self.received_data_average_peak + data.len() as i64) / 8;
        }
        result
    }

    fn input_inner(&mut self, data: &[u8]) -> Result<(), KcpError> {
        let prev_una = self.snd_una;
        let mut maxack = 0u32;
        let mut flag = false;

        if data.len() < OVERHEAD as usize {
            return Err(KcpError::InvalidInput);
        }

        let mut offset = 0usize;
        while data.len() - offset >= OVERHEAD as usize {
            let header = &data[offset..offset + OVERHEAD as usize];
            let conv = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
            if conv != self.conv {
                return Err(KcpError::InvalidInput);
            }
            let cmd = header[4];
            let frg = header[5];
            let wnd = u16::from_le_bytes([header[6], header[7]]) as u32;
            let ts = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
            let sn = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
            let una = u32::from_le_bytes([header[16], header[17], header[18], header[19]]);
            let len = u32::from_le_bytes([header[20], header[21], header[22], header[23]]);

            offset += OVERHEAD as usize;
            if data.len() - offset < len as usize {
                return Err(KcpError::InvalidInput);
            }
            if !matches!(cmd, CMD_PUSH | CMD_ACK | CMD_WASK | CMD_WINS) {
                return Err(KcpError::InvalidInput);
            }

            self.rmt_wnd = wnd;
            self.parse_una(una);
            self.shrink_buf();

            match cmd {
                CMD_ACK => {
                    if self.current >= ts {
                        let rtt = timediff(self.current, ts);
                        self.update_ack(rtt);
                    }
                    self.parse_ack(sn);
                    self.shrink_buf();
                    if !flag {
                        flag = true;
                        maxack = sn;
                    } else if sn > maxack {
                        maxack = sn;
                    }
                }
                CMD_PUSH => {
                    if sn < self.rcv_nxt.wrapping_add(self.rcv_wnd) {
                        self.acklist.push((sn, ts));
                        if sn >= self.rcv_nxt {
                            let mut seg = Segment::with_size(len as usize);
                            seg.conv = conv;
                            seg.cmd = cmd;
                            seg.frg = frg;
                            seg.wnd = wnd;
                            seg.ts = ts;
                            seg.sn = sn;
                            seg.una = una;
                            seg.len = len;
                            seg.data
                                .copy_from_slice(&data[offset..offset + len as usize]);
                            self.parse_data(seg);
                        }
                    }
                }
                CMD_WASK => self.probe |= ASK_TELL,
                CMD_WINS => {}
                _ => return Err(KcpError::InvalidInput),
            }

            offset += len as usize;
        }

        if flag {
            self.parse_fastack(maxack);
        }

        if self.snd_una > prev_una && self.cwnd < self.rmt_wnd {
            let mss = self.mss;
            if self.cwnd < self.ssthresh {
                self.cwnd += 1;
                self.incr = self.incr.wrapping_add(mss);
            } else {
                if self.incr < mss {
                    self.incr = mss;
                }
                self.incr = self
                    .incr
                    .wrapping_add((mss.wrapping_mul(mss)) / self.incr)
                    .wrapping_add(mss / 16);
                if (self.cwnd + 1).wrapping_mul(mss) <= self.incr {
                    self.cwnd = (self.incr + mss - 1) / mss.max(1);
                }
            }
            if self.cwnd > self.rmt_wnd {
                self.cwnd = self.rmt_wnd;
                self.incr = self.rmt_wnd.wrapping_mul(mss);
            }
        }

        Ok(())
    }

    fn update_ack(&mut self, rtt: i32) {
        if self.rx_srtt == 0 {
            self.rx_srtt = rtt;
            self.rx_rttval = rtt / 2;
        } else {
            let mut delta = rtt - self.rx_srtt;
            if delta < 0 {
                delta = -delta;
            }
            self.rx_rttval = (3 * self.rx_rttval + delta) / 4;
            self.rx_srtt = (7 * self.rx_srtt + rtt) / 8;
            if self.rx_srtt < 1 {
                self.rx_srtt = 1;
            }
        }
        let rto = self.rx_srtt + (self.interval as i32).max(4 * self.rx_rttval);
        self.rx_rto = rto.clamp(self.rx_minrto, RTO_MAX as i32);
    }

    fn shrink_buf(&mut self) {
        self.snd_una = if let Some((&sn, _)) = self.snd_buf.first_key_value() {
            sn
        } else {
            self.snd_nxt
        };
    }

    fn drop_index(&mut self, resendts: u32, fastack: u32, sn: u32) {
        if let Some(set) = self.resendts_buf.get_mut(&resendts) {
            set.remove(&sn);
        }
        if let Some(set) = self.fastack_buf.get_mut(&fastack) {
            set.remove(&sn);
        }
        if self
            .resendts_buf
            .get(&resendts)
            .is_some_and(BTreeSet::is_empty)
        {
            self.resendts_buf.remove(&resendts);
        }
        if self
            .fastack_buf
            .get(&fastack)
            .is_some_and(BTreeSet::is_empty)
        {
            self.fastack_buf.remove(&fastack);
        }
    }

    fn parse_ack(&mut self, sn: u32) {
        if sn < self.snd_una || sn >= self.snd_nxt {
            return;
        }
        if let Some(seg) = self.snd_buf.remove(&sn) {
            self.drop_index(seg.resendts, seg.fastack, sn);
        }
    }

    fn parse_una(&mut self, una: u32) {
        let keys: Vec<u32> = self.snd_buf.range(..una).map(|(&sn, _)| sn).collect();
        for sn in keys {
            if let Some(seg) = self.snd_buf.remove(&sn) {
                self.drop_index(seg.resendts, seg.fastack, sn);
            }
        }
    }

    fn parse_fastack(&mut self, sn: u32) {
        if sn < self.snd_una || sn >= self.snd_nxt {
            return;
        }

        let targets: Vec<u32> = self
            .snd_buf
            .range(..=sn)
            .filter(|(seg_sn, _)| **seg_sn != sn)
            .map(|(&seg_sn, _)| seg_sn)
            .collect();

        for seg_sn in targets {
            if let Some(seg) = self.snd_buf.get_mut(&seg_sn) {
                let old_fastack = seg.fastack;
                seg.fastack += 1;
                let new_fastack = seg.fastack;
                if let Some(set) = self.fastack_buf.get_mut(&old_fastack) {
                    set.remove(&seg_sn);
                    if set.is_empty() {
                        self.fastack_buf.remove(&old_fastack);
                    }
                }
                self.fastack_buf
                    .entry(new_fastack)
                    .or_default()
                    .insert(seg_sn);
            }
        }
    }

    fn parse_data(&mut self, newseg: Segment) {
        let sn = newseg.sn;
        if sn >= self.rcv_nxt.wrapping_add(self.rcv_wnd) || sn < self.rcv_nxt {
            return;
        }
        self.rcv_buf.entry(sn).or_insert(newseg);

        while let Some((&head_sn, _)) = self.rcv_buf.first_key_value() {
            if head_sn == self.rcv_nxt && self.rcv_queue.len() < self.rcv_wnd as usize {
                if let Some(seg) = self.rcv_buf.remove(&head_sn) {
                    self.rcv_queue.push_back(seg);
                    self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                }
            } else {
                break;
            }
        }
    }

    fn get_wnd_unused(&self) -> u32 {
        if self.rcv_queue.len() < self.rcv_wnd as usize {
            self.rcv_wnd - self.rcv_queue.len() as u32
        } else {
            0
        }
    }

    fn emit(&mut self, output: &SharedOutput, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.sent_data_average_peak = (7 * self.sent_data_average_peak + data.len() as i64) / 8;
        let _ = output(data);
    }

    fn encode_segment(seg: &Segment) -> [u8; OVERHEAD as usize] {
        let mut header = [0u8; OVERHEAD as usize];
        header[0..4].copy_from_slice(&seg.conv.to_le_bytes());
        header[4] = seg.cmd;
        header[5] = seg.frg;
        header[6..8].copy_from_slice(&(seg.wnd as u16).to_le_bytes());
        header[8..12].copy_from_slice(&seg.ts.to_le_bytes());
        header[12..16].copy_from_slice(&seg.sn.to_le_bytes());
        header[16..20].copy_from_slice(&seg.una.to_le_bytes());
        header[20..24].copy_from_slice(&seg.len.to_le_bytes());
        header
    }

    /// 把段写入当前输出缓冲；缓冲满时先发出去
    fn send_out(
        &mut self,
        output: &SharedOutput,
        buffer: &mut Vec<u8>,
        seg: &Segment,
    ) -> Result<(), KcpError> {
        let need = OVERHEAD as usize + seg.data.len();
        if buffer.len() + need > self.mtu as usize && !buffer.is_empty() {
            let data = std::mem::take(buffer);
            self.emit(output, &data);
        }
        buffer.extend_from_slice(&Self::encode_segment(seg));
        buffer.extend_from_slice(&seg.data);
        if seg.xmit >= self.dead_link {
            self.state = u32::MAX;
        }
        Ok(())
    }

    /// flush 待发送数据（对应 `ikcp_flush`）
    pub fn flush(&mut self, mut current: u32) -> Result<(), KcpError> {
        if self.updated == 0 {
            return Ok(());
        }
        if current == 0 {
            current = self.current;
        } else {
            self.current = current;
        }

        let Some(output) = self.output.clone() else {
            return Ok(());
        };

        let mut buffer: Vec<u8> = Vec::with_capacity(self.mtu as usize);
        let mut change = false;
        let mut lost = false;

        let wnd_unused = self.get_wnd_unused();
        let mut ack_seg = Segment {
            conv: self.conv,
            cmd: CMD_ACK,
            wnd: wnd_unused,
            una: self.rcv_nxt,
            ..Segment::default()
        };

        // 1. 发送 ACK
        let acks = std::mem::take(&mut self.acklist);
        for (ack_sn, ack_ts) in acks {
            if buffer.len() + OVERHEAD as usize > self.mtu as usize && !buffer.is_empty() {
                let data = std::mem::take(&mut buffer);
                self.emit(&output, &data);
            }
            ack_seg.sn = ack_sn;
            ack_seg.ts = ack_ts;
            buffer.extend_from_slice(&Self::encode_segment(&ack_seg));
        }

        // 2. 窗口探测
        if self.rmt_wnd == 0 {
            if self.probe_wait == 0 {
                self.probe_wait = PROBE_INIT;
                self.ts_probe = self.current.wrapping_add(self.probe_wait);
            } else if self.current >= self.ts_probe {
                if self.probe_wait < PROBE_INIT {
                    self.probe_wait = PROBE_INIT;
                }
                self.probe_wait += self.probe_wait / 2;
                self.probe_wait = self.probe_wait.min(PROBE_LIMIT);
                self.ts_probe = self.current.wrapping_add(self.probe_wait);
                self.probe |= ASK_SEND;
            }
        } else {
            self.ts_probe = 0;
            self.probe_wait = 0;
        }

        if self.probe & ASK_SEND != 0 {
            ack_seg.cmd = CMD_WASK;
            if buffer.len() + OVERHEAD as usize > self.mtu as usize && !buffer.is_empty() {
                let data = std::mem::take(&mut buffer);
                self.emit(&output, &data);
            }
            buffer.extend_from_slice(&Self::encode_segment(&ack_seg));
        }
        if self.probe & ASK_TELL != 0 {
            ack_seg.cmd = CMD_WINS;
            if buffer.len() + OVERHEAD as usize > self.mtu as usize && !buffer.is_empty() {
                let data = std::mem::take(&mut buffer);
                self.emit(&output, &data);
            }
            buffer.extend_from_slice(&Self::encode_segment(&ack_seg));
        }
        self.probe = 0;

        // 3. 计算窗口 / 重传参数
        let mut cwnd = self.snd_wnd.min(self.rmt_wnd);
        if self.nocwnd == 0 {
            cwnd = cwnd.min(self.cwnd);
        }
        let resent = if self.fastresend > 0 {
            self.fastresend as u32
        } else {
            u32::MAX
        };
        let rtomin = if self.nodelay == 0 {
            (self.rx_rto >> 3) as u32
        } else {
            0
        };

        // 4. 超时重传：resendts_buf 按时间升序
        let mut resend_list: Vec<u32> = Vec::new();
        let mut expired_keys: Vec<u32> = Vec::new();
        for (&resend_ts, segs) in &self.resendts_buf {
            if segs.is_empty() {
                expired_keys.push(resend_ts);
                continue;
            }
            if current < resend_ts {
                break;
            }
            resend_list.extend(segs.iter().copied());
        }
        for key in expired_keys {
            self.resendts_buf.remove(&key);
        }

        for sn in resend_list {
            let Some(mut seg) = self.snd_buf.remove(&sn) else {
                continue;
            };
            // 旧 resendts 索引
            let old_resendts = seg.resendts;
            if let Some(set) = self.resendts_buf.get_mut(&old_resendts) {
                set.remove(&sn);
                if set.is_empty() {
                    self.resendts_buf.remove(&old_resendts);
                }
            }

            seg.xmit += 1;
            if self.nodelay == 0 {
                seg.rto = seg.rto.wrapping_add(seg.rto.max(self.rx_rto as u32));
            } else {
                let step = if self.nodelay < 2 {
                    seg.rto as i32
                } else {
                    self.rx_rto
                };
                seg.rto = seg.rto.wrapping_add((step / 2) as u32);
            }
            seg.resendts = current.wrapping_add(seg.rto);
            lost = true;

            seg.ts = current;
            seg.wnd = ack_seg.wnd;
            seg.una = self.rcv_nxt;
            self.resendts_buf
                .entry(seg.resendts)
                .or_default()
                .insert(sn);
            self.send_out(&output, &mut buffer, &seg)?;
            self.snd_buf.insert(sn, seg);
        }

        // 5. 快速重传：fastack_buf 按 fastack 降序
        let fastack_keys: Vec<u32> = self.fastack_buf.keys().copied().collect();
        for fast_ack in fastack_keys.into_iter().rev() {
            if fast_ack < resent {
                break;
            }
            let sns: Vec<u32> = match self.fastack_buf.get(&fast_ack) {
                Some(set) if !set.is_empty() => set.iter().copied().collect(),
                _ => continue,
            };

            for sn in sns {
                let Some(mut seg) = self.snd_buf.remove(&sn) else {
                    if let Some(set) = self.fastack_buf.get_mut(&fast_ack) {
                        set.remove(&sn);
                    }
                    continue;
                };

                if seg.xmit as i32 <= self.fastlimit || self.fastlimit <= 0 {
                    let old_resendts = seg.resendts;
                    seg.xmit += 1;
                    seg.fastack = 0;
                    seg.resendts = current.wrapping_add(seg.rto);
                    change = true;

                    if let Some(set) = self.fastack_buf.get_mut(&fast_ack) {
                        set.remove(&sn);
                        if set.is_empty() {
                            self.fastack_buf.remove(&fast_ack);
                        }
                    }
                    self.fastack_buf.entry(0).or_default().insert(sn);

                    if let Some(set) = self.resendts_buf.get_mut(&old_resendts) {
                        set.remove(&sn);
                        if set.is_empty() {
                            self.resendts_buf.remove(&old_resendts);
                        }
                    }
                    self.resendts_buf
                        .entry(seg.resendts)
                        .or_default()
                        .insert(sn);

                    seg.ts = current;
                    seg.wnd = ack_seg.wnd;
                    seg.una = self.rcv_nxt;
                    self.send_out(&output, &mut buffer, &seg)?;
                }
                self.snd_buf.insert(sn, seg);
            }
        }

        // 6. 从 snd_queue 移到 snd_buf 并发送
        while self.snd_nxt < self.snd_una.wrapping_add(cwnd) && !self.snd_queue.is_empty() {
            let Some(mut seg) = self.snd_queue.pop_front() else {
                break;
            };
            seg.conv = self.conv;
            seg.cmd = CMD_PUSH;
            seg.wnd = ack_seg.wnd;
            seg.ts = current;
            seg.sn = self.snd_nxt;
            self.snd_nxt = self.snd_nxt.wrapping_add(1);
            seg.una = self.rcv_nxt;
            seg.resendts = current
                .wrapping_add(self.rx_rto as u32)
                .wrapping_add(rtomin);
            seg.rto = self.rx_rto as u32;
            seg.fastack = 0;
            seg.xmit = 1;

            self.resendts_buf
                .entry(seg.resendts)
                .or_default()
                .insert(seg.sn);
            self.fastack_buf.entry(0).or_default().insert(seg.sn);
            self.send_out(&output, &mut buffer, &seg)?;
            self.snd_buf.insert(seg.sn, seg);
        }

        // 7. 发出剩余数据
        if !buffer.is_empty() {
            let data = std::mem::take(&mut buffer);
            self.emit(&output, &data);
        }

        // 8. 拥塞窗口更新
        if change {
            let inflight = self.snd_nxt.wrapping_sub(self.snd_una);
            self.ssthresh = (inflight / 2).max(THRESH_MIN);
            self.cwnd = self.ssthresh.wrapping_add(resent);
            self.incr = self.cwnd.wrapping_mul(self.mss);
        }
        if lost {
            self.ssthresh = (cwnd / 2).max(THRESH_MIN);
            self.cwnd = 1;
            self.incr = self.mss;
        }
        if self.cwnd < 1 {
            self.cwnd = 1;
            self.incr = self.mss;
        }

        Ok(())
    }

    /// 定时驱动（对应 `ikcp_update`），返回 slap
    pub fn update(&mut self, current: u32) -> i32 {
        self.current = current;
        if self.updated == 0 {
            self.updated = 1;
            self.ts_flush = current;
        }

        let mut slap = timediff(self.current, self.ts_flush);
        if !(-10_000..10_000).contains(&slap) {
            self.ts_flush = self.current;
            slap = 0;
        }

        if slap >= 0 {
            self.ts_flush = self.ts_flush.wrapping_add(self.interval);
            if self.current >= self.ts_flush {
                self.ts_flush = self.current.wrapping_add(self.interval);
            }
            let _ = self.flush(0);
        }
        slap
    }

    /// 下次需要驱动的时刻（对应 `ikcp_check`）
    pub fn check(&self, current: u32) -> u32 {
        if self.updated == 0 {
            return current;
        }

        let mut ts_flush = self.ts_flush;
        if !(-10_000..10_000).contains(&timediff(current, ts_flush)) {
            ts_flush = current;
        }
        if current >= ts_flush {
            return current;
        }

        let tm_flush = timediff(ts_flush, current);
        let mut tm_packet = i32::MAX;

        if let Some((&resend_ts, segs)) = self.resendts_buf.first_key_value() {
            if !segs.is_empty() {
                let diff = timediff(resend_ts, current);
                if diff <= 0 {
                    return current;
                }
                if diff < tm_packet {
                    tm_packet = diff;
                }
            }
        }

        let mut minimal = tm_packet.min(tm_flush) as u32;
        if minimal >= self.interval {
            minimal = self.interval;
        }
        current.wrapping_add(minimal)
    }

    /// `flush` + `check`（对应 C++ wrapper 的 `Refresh`）
    pub fn refresh(&mut self, current: u32) -> u32 {
        let _ = self.flush(current);
        self.check(current)
    }

    /// `update` + `check`（对应 C++ wrapper 的 `UpdateCheck`）
    pub fn update_check(&mut self, current: u32) -> u32 {
        self.update(current);
        self.check(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn make_kcp() -> (Kcp, Arc<Mutex<Vec<Vec<u8>>>>) {
        let packets = Arc::new(Mutex::new(Vec::new()));
        let sink = packets.clone();
        let mut kcp = Kcp::new(KcpConfig {
            conv: 0x0102_0304,
            mtu: 1400,
            nodelay: 1,
            interval: 10,
            resend: 2,
            no_congestion_control: true,
            send_window: 32,
            receive_window: 128,
            stream_mode: false,
        });
        kcp.set_output(Box::new(move |data: &[u8]| {
            sink.lock().unwrap().push(data.to_vec());
            Ok(())
        }));
        (kcp, packets)
    }

    #[test]
    fn default_config_matches_cpp() {
        let config = KcpConfig::default();
        assert_eq!(config.send_window, 1024);
        assert_eq!(config.receive_window, 1024);
    }

    #[test]
    fn conv_roundtrip() {
        let kcp = Kcp::new(KcpConfig {
            conv: 0x1234_5678,
            ..Default::default()
        });
        assert_eq!(kcp.conv(), 0x1234_5678);
        assert_eq!(Kcp::peek_conv(&[0x78, 0x56, 0x34, 0x12]), Some(0x1234_5678));
    }

    #[test]
    fn update_without_data_emits_nothing() {
        let (mut kcp, packets) = make_kcp();
        assert_eq!(kcp.update(1000), 0);
        assert!(packets.lock().unwrap().is_empty());
    }

    #[test]
    fn send_then_update_emits_one_segment() {
        let (mut kcp, packets) = make_kcp();
        kcp.update(1000);
        kcp.send(b"hello").unwrap();
        assert_eq!(kcp.waiting_for_send(), 1);
        kcp.update(1010);

        let packets = packets.lock().unwrap();
        assert_eq!(packets.len(), 1);
        let packet = &packets[0];
        assert_eq!(packet.len(), OVERHEAD as usize + 5);
        assert_eq!(&packet[..4], &0x0102_0304u32.to_le_bytes());
        assert_eq!(packet[4], CMD_PUSH);
        assert_eq!(packet[5], 0); // frg
        assert_eq!(packet[8..12], 1010u32.to_le_bytes()); // ts
        assert_eq!(packet[12..16], 0u32.to_le_bytes()); // sn
        assert_eq!(packet[16..20], 0u32.to_le_bytes()); // una
        assert_eq!(packet[20..24], 5u32.to_le_bytes()); // len
        assert_eq!(&packet[24..], b"hello");
        assert_eq!(kcp.waiting_for_send(), 1); // 已进入 snd_buf
    }

    #[test]
    fn fragmentation_sets_frg_order() {
        let (mut kcp, packets) = make_kcp();
        kcp.set_mtu(100).unwrap();
        kcp.update(1000);
        let data = vec![0xAB; 200];
        kcp.send(&data).unwrap();
        kcp.update(1010);

        let packets = packets.lock().unwrap();
        assert!(packets.len() >= 2);
        let frg: Vec<u8> = packets.iter().map(|packet| packet[5]).collect();
        // mss = 76，200 字节拆成 3 段：frg = 2, 1, 0
        assert_eq!(frg, vec![2, 1, 0]);
    }

    #[test]
    fn input_ack_removes_from_snd_buf() {
        let (mut kcp, packets) = make_kcp();
        kcp.update(1000);
        kcp.send(b"hello").unwrap();
        kcp.update(1010);
        let sent_ts = {
            let packets = packets.lock().unwrap();
            u32::from_le_bytes([packets[0][8], packets[0][9], packets[0][10], packets[0][11]])
        };

        let mut ack = [0u8; OVERHEAD as usize];
        ack[..4].copy_from_slice(&0x0102_0304u32.to_le_bytes());
        ack[4] = CMD_ACK;
        ack[8..12].copy_from_slice(&sent_ts.to_le_bytes());
        ack[12..16].copy_from_slice(&0u32.to_le_bytes());
        // 让 current 前进 20ms，使 RTT 采样非 0
        kcp.update(1030);
        kcp.input(&ack).unwrap();
        assert_eq!(kcp.waiting_for_send(), 0);
        assert!(kcp.rx_srtt() > 0);
    }

    #[test]
    fn receive_reassembles_fragments() {
        // 直接用本端编码的段喂给另一个 KCP
        let (mut sender, packets) = make_kcp();
        sender.set_mtu(100).unwrap();
        sender.update(1000);
        let data = vec![0x5A; 200];
        sender.send(&data).unwrap();
        sender.update(1010);

        let (mut receiver, _) = make_kcp();
        receiver.update(1000);
        for packet in packets.lock().unwrap().iter() {
            receiver.input(packet).unwrap();
        }
        let mut buffer = vec![0u8; 300];
        let size = receiver.receive(&mut buffer).unwrap();
        assert_eq!(size, data.len());
        assert_eq!(&buffer[..size], &data[..]);
    }

    #[test]
    fn rto_retransmits_without_ack() {
        let (mut kcp, packets) = make_kcp();
        kcp.update(1000);
        kcp.send(b"hello").unwrap();
        kcp.update(1010);
        assert_eq!(packets.lock().unwrap().len(), 1);

        // 初始 rx_rto = 200，nodelay=1 -> rtomin = 0，resendts = 1210
        kcp.update(1210);
        assert_eq!(packets.lock().unwrap().len(), 2);
        assert!(!kcp.is_dead());
    }

    #[test]
    fn window_size_has_rcv_floor() {
        let mut kcp = Kcp::new(KcpConfig::default());
        kcp.set_window_size(0, 8);
        assert_eq!(kcp.window_sizes(), (1024, 128));
    }

    #[test]
    fn bandwidth_reset_uses_min_rtt() {
        let mut kcp = Kcp::new(KcpConfig::default());
        kcp.set_bandwidth(1_000_000, 2_000_000);
        kcp.reset_window_values(100);
        let (snd, rcv) = kcp.window_sizes();
        assert!(snd >= 32);
        assert!(rcv >= 32);
    }

    /// 与 C++ 版同一调用序列的输出逐字节对撞。
    ///
    /// C++ 探针：`KCP::KCP` + `SetMTU(1400)` + `SetWindowSize(32,128)` +
    /// `NoDelay(1,10,2,1)`，依次 Update(1000) / Send("hello") / Update(1010) /
    /// Input(ACK sn=0) / Update(1030) / Update(1040) / SetMTU(100) /
    /// Send(200 字节) / Update(1050)。期望值由该探针打印。
    #[test]
    fn output_matches_cpp_transcript() {
        let (mut kcp, packets) = make_kcp();
        kcp.update(1000);
        kcp.send(b"hello").unwrap();
        kcp.update(1010);

        let mut ack = [0u8; OVERHEAD as usize];
        ack[..4].copy_from_slice(&0x0102_0304u32.to_le_bytes());
        ack[4] = CMD_ACK;
        ack[6..8].copy_from_slice(&128u16.to_le_bytes());
        ack[8..12].copy_from_slice(&1010u32.to_le_bytes());
        kcp.input(&ack).unwrap();
        kcp.update(1030);
        kcp.update(1040);

        kcp.set_mtu(100).unwrap();
        let big: Vec<u8> = (0..200u16).map(|value| value as u8).collect();
        kcp.send(&big).unwrap();
        kcp.update(1050);

        let actual: Vec<String> = packets
            .lock()
            .unwrap()
            .iter()
            .map(hex::encode_upper)
            .collect();
        let expected = vec![
            "0403020151008000F203000000000000000000000500000068656C6C6F",
            concat!(
                "04030201510280001A04000001000000000000004C000000",
                "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F",
                "202122232425262728292A2B2C2D2E2F303132333435363738393A3B3C3D3E3F",
                "404142434445464748494A4B"
            ),
            concat!(
                "04030201510180001A04000002000000000000004C000000",
                "4C4D4E4F505152535455565758595A5B5C5D5E5F606162636465666768696A6B",
                "6C6D6E6F707172737475767778797A7B7C7D7E7F808182838485868788898A8B8C",
                "8D8E8F9091929394959697"
            ),
            concat!(
                "04030201510080001A040000030000000000000030000000",
                "98999A9B9C9D9E9FA0A1A2A3A4A5A6A7A8A9AAABACADAEAF",
                "B0B1B2B3B4B5B6B7B8B9BABBBCBDBEBFC0C1C2C3C4C5C6C7"
            ),
        ];
        assert_eq!(actual, expected);
    }

    /// 10% 丢包的进程内回环：验证重传、ACK、窗口推进能把数据完整送达。
    #[test]
    fn lossy_loopback_delivers_all_data() {
        let (mut sender, sender_out) = make_kcp();
        let (mut receiver, receiver_out) = make_kcp();

        let messages: Vec<Vec<u8>> = (0..40u8)
            .map(|index| vec![index; 100 + index as usize * 7])
            .collect();
        let expected: Vec<u8> = messages.concat();
        for message in &messages {
            sender.send(message).unwrap();
        }

        let mut now = 1000u32;
        let mut packet_sequence = 0u64;
        let mut received = Vec::new();
        let mut buffer = vec![0u8; 4096];

        while received.len() < expected.len() && now < 60_000 {
            let packets: Vec<Vec<u8>> = sender_out.lock().unwrap().drain(..).collect();
            for packet in packets {
                packet_sequence += 1;
                if packet_sequence % 9 != 0 {
                    receiver.input(&packet).unwrap();
                }
            }

            let packets: Vec<Vec<u8>> = receiver_out.lock().unwrap().drain(..).collect();
            for packet in packets {
                packet_sequence += 1;
                if packet_sequence % 9 != 0 {
                    sender.input(&packet).unwrap();
                }
            }

            while let Ok(size) = receiver.receive(&mut buffer) {
                received.extend_from_slice(&buffer[..size]);
            }

            sender.update(now);
            receiver.update(now);
            now += 10;
        }

        assert_eq!(received.len(), expected.len(), "60 秒内应完成传输");
        assert_eq!(received, expected);
        assert!(!sender.is_dead());
        assert!(!receiver.is_dead());
    }

    /// 超时重传的包内容与 C++ 一致（C++ 的 `unordered_map` 迭代顺序不稳定，
    /// 这里按 SN 排序后比较）。
    #[test]
    fn retransmission_matches_cpp_packets() {
        let (mut kcp, packets) = make_kcp();
        kcp.update(1000);
        kcp.send(b"hello").unwrap();
        kcp.update(1010);

        let mut ack = [0u8; OVERHEAD as usize];
        ack[..4].copy_from_slice(&0x0102_0304u32.to_le_bytes());
        ack[4] = CMD_ACK;
        ack[6..8].copy_from_slice(&128u16.to_le_bytes());
        ack[8..12].copy_from_slice(&1010u32.to_le_bytes());
        kcp.input(&ack).unwrap();
        kcp.update(1030);
        kcp.update(1040);

        kcp.set_mtu(100).unwrap();
        let big: Vec<u8> = (0..200u16).map(|value| value as u8).collect();
        kcp.send(&big).unwrap();
        kcp.update(1050);
        kcp.update(1250);

        let mut packets = packets.lock().unwrap().clone();
        // 丢掉首次发送的 4 个包，只保留 1250ms 的重传
        packets.drain(..4);
        let mut actual: Vec<String> = packets.iter().map(hex::encode_upper).collect();
        actual.sort();

        let mut expected = vec![
            concat!(
                "0403020151008000E2040000030000000000000030000000",
                "98999A9B9C9D9E9FA0A1A2A3A4A5A6A7A8A9AAABACADAEAF",
                "B0B1B2B3B4B5B6B7B8B9BABBBCBDBEBFC0C1C2C3C4C5C6C7"
            ),
            concat!(
                "0403020151018000E204000002000000000000004C000000",
                "4C4D4E4F505152535455565758595A5B5C5D5E5F606162636465666768696A6B",
                "6C6D6E6F707172737475767778797A7B7C7D7E7F808182838485868788898A8B8C",
                "8D8E8F9091929394959697"
            ),
            concat!(
                "0403020151028000E204000001000000000000004C000000",
                "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F",
                "202122232425262728292A2B2C2D2E2F303132333435363738393A3B3C3D3E3F",
                "404142434445464748494A4B"
            ),
        ];
        expected.sort();
        assert_eq!(actual, expected);
    }
}
