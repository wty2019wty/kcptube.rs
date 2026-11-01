// SPDX-License-Identifier: GPL-3.0-only

//! 两段式状态摘要（对应 C++ `relay_mode::loop_get_status`）。
//!
//! ```text
//! [时间]Summary of <config>
//! [Client <-> This] receive (raw): ..., receive (inner): ..., send (inner): ..., send (raw): ..., fec recover: ...
//! [This <-> Remote] receive (raw): ..., receive (inner): ..., send (inner): ..., send (raw): ..., fec recover: ...
//! [Client <-> This] KCP#<conv> average latency: <srtt> ms
//! [This <-> Remote] KCP#<conv> average latency: <srtt> ms
//!
//! ```
//!
//! 与 C++ 的差异：C++ 把 ingress 的 `fec recover` 误写成
//! `forwarder_status_counters.fec_recovery_count`（`relay.cpp:2017`），
//! Rust 用各段自己的计数器（已登记在 `docs/dev/relay-plan.md` §2.9）。

use crate::state::{HandshakeEntry, RelaySession, RelayState};
use kcptube_logging::{
    LOGGING_GAP_SECONDS, StatusCounters, print_status_to_file, time_string_with_brackets,
    to_speed_unit,
};
use kcptube_transport::now_secs;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// 单段的已格式化数值。
pub(crate) struct SegmentStatus<'a> {
    pub receive_raw: &'a str,
    pub receive_inner: &'a str,
    pub send_inner: &'a str,
    pub send_raw: &'a str,
    pub fec_recover: u64,
}

/// 拼出完整摘要（末尾留一个空行）。
pub(crate) fn format_status(
    config_filename: &str,
    listener: &SegmentStatus<'_>,
    forwarder: &SegmentStatus<'_>,
    latencies: &[(u32, i32, i32)],
) -> String {
    let mut output = format!(
        "{}Summary of {config_filename}\n",
        time_string_with_brackets()
    );
    output.push_str(&format!(
        "[Client <-> This] receive (raw): {}, receive (inner): {}, send (inner): {}, send (raw): {}, fec recover: {}\n",
        listener.receive_raw,
        listener.receive_inner,
        listener.send_inner,
        listener.send_raw,
        listener.fec_recover
    ));
    output.push_str(&format!(
        "[This <-> Remote] receive (raw): {}, receive (inner): {}, send (inner): {}, send (raw): {}, fec recover: {}\n",
        forwarder.receive_raw,
        forwarder.receive_inner,
        forwarder.send_inner,
        forwarder.send_raw,
        forwarder.fec_recover
    ));
    for (conv, ingress_srtt, egress_srtt) in latencies {
        output.push_str(&format!(
            "[Client <-> This] KCP#{conv} average latency: {ingress_srtt} ms\n"
        ));
        output.push_str(&format!(
            "[This <-> Remote] KCP#{conv} average latency: {egress_srtt} ms\n"
        ));
    }
    output.push('\n');
    output
}

impl RelayState {
    /// 每 `LOGGING_GAP_SECONDS` 秒输出一次；未配置 `log_status` 时跳过。
    pub(crate) async fn maybe_log_status(&self) {
        if !self.settings.has_status_log() {
            return;
        }
        let now = now_secs();
        if now < self.next_status_at.load(Ordering::Relaxed) {
            return;
        }
        self.next_status_at
            .store(now + LOGGING_GAP_SECONDS as i64, Ordering::Relaxed);

        let sessions: Vec<Arc<RelaySession>> =
            self.sessions.lock().await.values().cloned().collect();
        let handshakes: Vec<Arc<HandshakeEntry>> =
            self.handshakes.lock().await.values().cloned().collect();

        // `send (raw)` 用加密后的字节数聚合（C++ 在发送回调里累加，等价）
        let mut listener_send_raw = 0u64;
        let mut forwarder_send_raw = 0u64;
        for session in &sessions {
            listener_send_raw += session.ingress.take_output_bytes();
            forwarder_send_raw += session.egress.take_output_bytes();
        }
        for entry in &handshakes {
            listener_send_raw += entry.ingress.take_output_bytes();
            if let Some(egress) = entry.egress.lock().await.clone() {
                forwarder_send_raw += egress.take_output_bytes();
            }
        }

        let duration = LOGGING_GAP_SECONDS;
        let listener_receive_raw = to_speed_unit(
            StatusCounters::take(&self.listener_counters.ingress_raw_traffic),
            duration,
        );
        let listener_receive_inner = to_speed_unit(
            StatusCounters::take(&self.listener_counters.ingress_inner_traffic),
            duration,
        );
        let listener_send_inner = to_speed_unit(
            StatusCounters::take(&self.listener_counters.egress_inner_traffic),
            duration,
        );
        let listener_send_raw_text = to_speed_unit(listener_send_raw, duration);
        let listener_fec = StatusCounters::take(&self.listener_counters.fec_recovery_count);
        let listener = SegmentStatus {
            receive_raw: &listener_receive_raw,
            receive_inner: &listener_receive_inner,
            send_inner: &listener_send_inner,
            send_raw: &listener_send_raw_text,
            fec_recover: listener_fec,
        };

        let forwarder_receive_raw = to_speed_unit(
            StatusCounters::take(&self.forwarder_counters.ingress_raw_traffic),
            duration,
        );
        let forwarder_receive_inner = to_speed_unit(
            StatusCounters::take(&self.forwarder_counters.ingress_inner_traffic),
            duration,
        );
        let forwarder_send_inner = to_speed_unit(
            StatusCounters::take(&self.forwarder_counters.egress_inner_traffic),
            duration,
        );
        let forwarder_send_raw_text = to_speed_unit(forwarder_send_raw, duration);
        let forwarder_fec = StatusCounters::take(&self.forwarder_counters.fec_recovery_count);
        let forwarder = SegmentStatus {
            receive_raw: &forwarder_receive_raw,
            receive_inner: &forwarder_receive_inner,
            send_inner: &forwarder_send_inner,
            send_raw: &forwarder_send_raw_text,
            fec_recover: forwarder_fec,
        };

        let mut latencies = Vec::with_capacity(sessions.len());
        for session in &sessions {
            latencies.push((
                session.conv,
                session.ingress.rx_srtt().await,
                session.egress.rx_srtt().await,
            ));
        }

        let config_filename = self.settings.config_filename.display().to_string();
        let output = format_status(&config_filename, &listener, &forwarder, &latencies);
        let _ = print_status_to_file(&self.settings.log_status, &output);
        // 对应 C++ `std::cout << output_text << std::endl;`（末尾再补一个换行）
        println!("{output}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_format_matches_cpp() {
        let listener = SegmentStatus {
            receive_raw: "1 KiB/s",
            receive_inner: "2 KiB/s",
            send_inner: "3 KiB/s",
            send_raw: "4 KiB/s",
            fec_recover: 5,
        };
        let forwarder = SegmentStatus {
            receive_raw: "6 KiB/s",
            receive_inner: "7 KiB/s",
            send_inner: "8 KiB/s",
            send_raw: "9 KiB/s",
            fec_recover: 10,
        };
        let latencies = [(0x1234, 11, 12)];
        let output = format_status("relay.conf", &listener, &forwarder, &latencies);
        let lines: Vec<&str> = output.lines().collect();

        assert_eq!(lines.len(), 6);
        assert!(
            lines[0].ends_with("] Summary of relay.conf"),
            "{}",
            lines[0]
        );
        assert_eq!(
            lines[1],
            "[Client <-> This] receive (raw): 1 KiB/s, receive (inner): 2 KiB/s, send (inner): 3 KiB/s, send (raw): 4 KiB/s, fec recover: 5"
        );
        assert_eq!(
            lines[2],
            "[This <-> Remote] receive (raw): 6 KiB/s, receive (inner): 7 KiB/s, send (inner): 8 KiB/s, send (raw): 9 KiB/s, fec recover: 10"
        );
        assert_eq!(
            lines[3],
            "[Client <-> This] KCP#4660 average latency: 11 ms"
        );
        assert_eq!(
            lines[4],
            "[This <-> Remote] KCP#4660 average latency: 12 ms"
        );
        assert_eq!(lines[5], "");
    }
}
