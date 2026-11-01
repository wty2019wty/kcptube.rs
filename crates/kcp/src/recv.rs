// SPDX-License-Identifier: GPL-3.0-only

//! 接收侧：`receive` / `input` 与重组缓冲区。

use super::*;

impl Kcp {
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
}
