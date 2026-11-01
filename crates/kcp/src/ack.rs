// SPDX-License-Identifier: GPL-3.0-only

//! ACK 与重传索引维护（对应 ikcp 的 `parse_ack` / `parse_una` / `parse_fastack`）。

use super::*;

impl Kcp {
    pub(crate) fn update_ack(&mut self, rtt: i32) {
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

    pub(crate) fn shrink_buf(&mut self) {
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

    pub(crate) fn parse_ack(&mut self, sn: u32) {
        if sn < self.snd_una || sn >= self.snd_nxt {
            return;
        }
        if let Some(seg) = self.snd_buf.remove(&sn) {
            self.drop_index(seg.resendts, seg.fastack, sn);
        }
    }

    pub(crate) fn parse_una(&mut self, una: u32) {
        let keys: Vec<u32> = self.snd_buf.range(..una).map(|(&sn, _)| sn).collect();
        for sn in keys {
            if let Some(seg) = self.snd_buf.remove(&sn) {
                self.drop_index(seg.resendts, seg.fastack, sn);
            }
        }
    }

    pub(crate) fn parse_fastack(&mut self, sn: u32) {
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
}
