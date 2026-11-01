// SPDX-License-Identifier: GPL-3.0-only

//! 状态摘要日志（每 `LOGGING_GAP_SECONDS` 秒一次）。

use crate::state::{DataSession, HandshakeEntry, SocketState};
use kcptube_logging::{
    LOGGING_GAP_SECONDS, StatusCounters, StatusSummary, print_status_summary, to_speed_unit,
};
use kcptube_transport::now_secs;
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl SocketState {
    /// 状态摘要：每 `LOGGING_GAP_SECONDS` 秒一次
    ///
    /// 与 C++ 一致：只有配置了 `log_status`（即设置了 `log_path`）时才输出。
    pub(crate) async fn maybe_log_status(&self) {
        if !self.settings.has_status_log() {
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

        let config_filename = self.settings.config_filename.display().to_string();
        let summary = StatusSummary {
            config_filename: &config_filename,
            receive_raw: &receive_raw,
            receive_inner: &receive_inner,
            send_inner: &send_inner,
            send_raw: &send_raw,
            fec_recover,
            latencies: &latencies,
        };
        print_status_summary(&self.settings.log_status, &summary);
    }
}
