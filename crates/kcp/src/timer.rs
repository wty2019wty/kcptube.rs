// SPDX-License-Identifier: GPL-3.0-only

//! 定时驱动（对应 `ikcp_update` / `ikcp_check` 与 wrapper 的 `Refresh`）。

use super::*;

impl Kcp {
    /// 若尚未执行过第一次 `update`，先初始化内部时间基准。
    ///
    /// `flush` 在 `updated == 0` 时会直接返回，因此上层若要在首次
    /// `update` 之前主动 flush（`blast` 模式），需要先调用本方法。
    pub fn ensure_started(&mut self, current: u32) {
        if self.updated == 0 {
            self.update(current);
        }
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
