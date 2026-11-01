// SPDX-License-Identifier: GPL-3.0-only

//! 发送侧：`send` / 段编码 / `flush`（超时与快速重传）。

use super::*;

impl Kcp {
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
}
