// SPDX-License-Identifier: GPL-3.0-only

//! 配置与状态查询：构造、参数设置、窗口与队列状态。

use super::*;

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
}
