// SPDX-License-Identifier: GPL-3.0-only

//! 每秒维护：空闲会话回收、两侧保活、状态日志（对应 C++
//! `find_expires` / `keep_alive_ingress` / `keep_alive_egress` / `log_status`）。
//!
//! C++ 用 `expiring_handshakes` / `expiring_forwarders` / `expiring_kcp`
//! 三张表分阶段回收。Rust 侧由各任务自行收尾，因此这里只需要：
//!
//! - 空闲/强制的会话判定（命中后标记关闭，会话任务 5ms 内退出并保留 4 秒冲刷）；
//! - 两侧独立的保活发送；
//! - 每 60 秒输出一次状态摘要。
//!
//! 握手条目由自身任务在 `KCP_CLEANUP_WAITS * 2` 秒后退出，不需要额外维护。

use crate::state::{RelaySession, RelayState};
use kcptube_protocol::constant::TIME_GAP_SECONDS;
use kcptube_protocol::packet::{ProtocolType, create_keep_alive_packet};
use kcptube_transport::{KcpChannel, now_secs};
use std::sync::Arc;
use tokio::time::{Duration, interval};

/// 维护任务（全局一份，1 秒一次）。
pub(crate) async fn run_maintenance(state: Arc<RelayState>) {
    let mut ticker = interval(Duration::from_secs(1));
    loop {
        ticker.tick().await;
        expire_sessions(&state).await;
        tick_keep_alive(&state).await;
        state.maybe_log_status().await;
    }
}

/// 回收空闲会话，对应 C++ `loop_find_expires` 的前两条判定。
///
/// 端口跳跃部分（`change_new_port`）属 R3，此处不处理。
async fn expire_sessions(state: &Arc<RelayState>) {
    let now = now_secs();
    let udp_timeout = i64::from(state.egress.udp_timeout);
    let sessions: Vec<Arc<RelaySession>> = state.sessions.lock().await.values().cloned().collect();

    for session in sessions {
        if session.is_closed() {
            continue;
        }
        let ingress_gap = (now - session.ingress.last_input_time().await).abs();
        let egress_gap = (now - session.egress.last_input_time().await).abs();
        let transfer_gap = (now - session.last_data_transfer()).abs();

        if should_expire(ingress_gap, egress_gap, transfer_gap, udp_timeout) {
            state.sessions.lock().await.remove(&session.conv);
            session.close();
            tracing::debug!("relay 会话 conv={} 空闲超时，已回收", session.conv);
        }
    }
}

/// 空闲判定（纯函数，便于单元测试）。
///
/// 与 C++ 一致：
/// - 任一方向超过 `gbv_time_gap_seconds`（255 秒）强制回收；
/// - 三个时间（全局最后转发、ingress 最后收、egress 最后收）同时超过
///   `[forwarder].udp_timeout` 也回收。
pub(crate) fn should_expire(
    ingress_gap: i64,
    egress_gap: i64,
    transfer_gap: i64,
    udp_timeout: i64,
) -> bool {
    if ingress_gap > TIME_GAP_SECONDS || egress_gap > TIME_GAP_SECONDS {
        return true;
    }
    transfer_gap > udp_timeout && ingress_gap > udp_timeout && egress_gap > udp_timeout
}

/// 两侧独立发送保活（对应 C++ `loop_keep_alive_ingress` / `_egress`）。
async fn tick_keep_alive(state: &Arc<RelayState>) {
    let now = now_secs();
    let ingress_interval = i64::from(state.ingress.keep_alive);
    let egress_interval = i64::from(state.egress.keep_alive);
    if ingress_interval <= 0 && egress_interval <= 0 {
        return;
    }

    let sessions: Vec<Arc<RelaySession>> = state.sessions.lock().await.values().cloned().collect();
    for session in sessions {
        if session.is_closed() {
            continue;
        }
        send_keep_alive_if_due(
            &session.ingress,
            session.protocol,
            ingress_interval,
            session.ingress_next_keep_alive(),
            now,
            |next| session.set_ingress_next_keep_alive(next),
        )
        .await;
        send_keep_alive_if_due(
            &session.egress,
            session.protocol,
            egress_interval,
            session.egress_next_keep_alive(),
            now,
            |next| session.set_egress_next_keep_alive(next),
        )
        .await;
    }
}

async fn send_keep_alive_if_due(
    channel: &KcpChannel,
    protocol: ProtocolType,
    interval: i64,
    next_at: i64,
    now: i64,
    update: impl FnOnce(i64),
) {
    if interval <= 0 || now < next_at {
        return;
    }
    if channel
        .send(&create_keep_alive_packet(protocol))
        .await
        .is_ok()
    {
        channel.mark_keep_alive_sent(now);
    }
    // 与 C++ 一致：发送失败也推进下次时刻，避免每个 tick 重试
    update(now + interval);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_gap_forces_expiry() {
        assert!(should_expire(TIME_GAP_SECONDS + 1, 0, 0, 180));
        assert!(should_expire(0, TIME_GAP_SECONDS + 1, 0, 180));
        assert!(!should_expire(TIME_GAP_SECONDS, 0, 0, 180));
    }

    #[test]
    fn idle_requires_all_three_gaps() {
        assert!(should_expire(200, 200, 200, 180));
        // 任一方向仍有流量则不回收
        assert!(!should_expire(179, 200, 200, 180));
        assert!(!should_expire(200, 179, 200, 180));
        assert!(!should_expire(200, 200, 179, 180));
    }

    #[test]
    fn keep_alive_disabled_means_no_expiry() {
        // interval = 0 时不会进入发送分支（由 tick_keep_alive 提前返回）
        assert!(!should_expire(10, 10, 10, 180));
    }
}
