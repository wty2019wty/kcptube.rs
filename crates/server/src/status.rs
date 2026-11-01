// SPDX-License-Identifier: GPL-3.0-only

//! 状态摘要日志（每 `LOGGING_GAP_SECONDS` 秒一次）。

use crate::state::{DataSession, HandshakeEntry, SocketState};
use kcptube_logging::{
    LOGGING_GAP_SECONDS, StatusCounters, print_status_to_file, time_string_with_brackets,
    to_speed_unit,
};
use kcptube_transport::now_secs;
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl SocketState {
    /// 状态摘要：每 `LOGGING_GAP_SECONDS` 秒一次
    ///
    /// 与 C++ 一致：只有配置了 `log_status`（即设置了 `log_path`）时才输出。
    pub(crate) async fn maybe_log_status(&self) {
        if self.settings.log_status.as_os_str().is_empty() {
            return;
        }
        let now = now_secs();
        if now < self.next_status_at.load(Ordering::SeqCst) {
            return;
        }
        self.next_status_at
            .store(now + LOGGING_GAP_SECONDS as i64, Ordering::SeqCst);

        let duration = LOGGING_GAP_SECONDS;
        let receive_raw = to_speed_unit(
            StatusCounters::take(&self.counters.ingress_raw_traffic),
            duration,
        );
        let receive_inner = to_speed_unit(
            StatusCounters::take(&self.counters.ingress_inner_traffic),
            duration,
        );
        let send_inner = to_speed_unit(
            StatusCounters::take(&self.counters.egress_inner_traffic),
            duration,
        );

        let sessions: Vec<(u32, Arc<DataSession>)> = self
            .sessions
            .lock()
            .await
            .iter()
            .map(|(conv, session)| (*conv, session.clone()))
            .collect();
        let mut send_raw_bytes = 0u64;
        let mut latencies = Vec::new();
        for (conv, session) in &sessions {
            send_raw_bytes += session.channel.take_output_bytes();
            latencies.push((*conv, session.channel.rx_srtt().await));
        }
        {
            let handshakes: Vec<Arc<HandshakeEntry>> =
                self.handshakes.lock().await.values().cloned().collect();
            for entry in handshakes {
                send_raw_bytes += entry.channel.take_output_bytes();
            }
        }
        let send_raw = to_speed_unit(send_raw_bytes, duration);
        let fec_recover = StatusCounters::take(&self.counters.fec_recovery_count);

        let mut output = format!(
            "{}Summary of {}\n",
            time_string_with_brackets(),
            self.settings.config_filename.display()
        );
        output.push_str(&format!(
            "receive (raw): {receive_raw}, receive (inner): {receive_inner}, send (inner): {send_inner}, send (raw): {send_raw}, fec recover: {fec_recover}\n"
        ));
        for (conv, srtt) in latencies {
            output.push_str(&format!("KCP#{conv} average latency: {srtt} ms\n"));
        }
        output.push('\n');

        let _ = print_status_to_file(&self.settings.log_status, &output);
        // 对应 C++ `std::cout << output_text << std::endl;`（末尾再补一个换行）
        println!("{output}");
    }
}
