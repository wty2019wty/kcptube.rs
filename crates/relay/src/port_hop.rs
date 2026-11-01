// SPDX-License-Identifier: GPL-3.0-only

//! relay 侧端口跳跃（对应 C++ `change_new_port` / `test_before_change` /
//! `switch_new_port`）。
//!
//! 只有 **egress（面向服务端）** 会主动跳跃；ingress 侧跟随客户端的源端口
//! 变化（由 `listener` 更新回包目标）。触发时机与 C++ 一致：每次成功转发
//! 数据后检查一次 `hopping_at`，而不是独立定时器——空闲连接不跳跃。
//!
//! 与 C++ 的两处有意差异：
//! - C++ 探测超时后不再重置 `hopping_timestamp`（一次失败即永久停止）；
//!   Rust 按 `dport_refresh` 重试。
//! - C++ 需要两次发送才完成切换（先探测、下次发送时 `switch_new_port`）；
//!   Rust 在探测成功后立即重绑，行为等价且更直接。

use crate::RelayError;
use crate::state::{RelaySession, RelayState};
use crate::util::random_pick;
use kcptube_transport::{
    PortHop, UdpEndpoint, drain_old_socket, now_secs, resolve_endpoints_filtered,
};
use std::sync::Arc;
use tokio::time::Duration;

/// 切换后旧 socket 继续收包的时间（对应 C++ `gbv_receiver_cleanup_waits / 2`）。
const OLD_SOCKET_DRAIN: Duration = Duration::from_secs(2);

/// 每次转发数据后调用：到点则后台探测新端口并切换。
pub(crate) fn maybe_hop(state: &Arc<RelayState>, session: &Arc<RelaySession>) {
    let refresh = i64::from(state.egress.dynamic_port_refresh);
    if refresh <= 0 {
        return;
    }
    let now = now_secs();
    if now < session.hopping_at() {
        return;
    }
    // 同一会话同时只允许一个探测
    if session.swap_hopping_testing(true) {
        return;
    }
    session.set_hopping_at(i64::MAX);

    let state = state.clone();
    let session = session.clone();
    tokio::spawn(async move {
        if session.is_closed() {
            session.swap_hopping_testing(false);
            return;
        }

        let outcome = probe(&state, &session).await;
        session.swap_hopping_testing(false);

        match outcome {
            Ok(Some(old_socket)) => {
                // probe 内部已 `rebind`，当前发送 socket 就是新 socket
                session.replace_egress_socket(session.egress.socket());
                state.log_message(&format!("relay 端口跳跃：conv={} 已切换", session.conv));
                let channel = session.egress.clone();
                tokio::spawn(async move {
                    drain_old_socket(channel, old_socket, OLD_SOCKET_DRAIN).await;
                });
            }
            Ok(None) => {
                tracing::debug!("relay conv={} 端口探测未成功", session.conv);
            }
            Err(error) => {
                tracing::warn!("relay conv={} 端口探测失败：{error}", session.conv);
            }
        }
        // 失败也按 dport_refresh 重试（C++ 会永久停止，见模块注释）
        session.set_hopping_at(now_secs() + refresh);
    });
}

async fn probe(
    state: &RelayState,
    session: &RelaySession,
) -> Result<Option<UdpEndpoint>, RelayError> {
    let host = random_pick(&state.egress.destination_address_list)
        .cloned()
        .ok_or_else(|| RelayError::Startup("destination_address is not set".to_string()))?;
    let port = random_pick(&state.egress.destination_ports)
        .copied()
        .ok_or_else(|| RelayError::Startup("destination_port is not set".to_string()))?;

    let candidates =
        resolve_endpoints_filtered(&host, port, state.egress.ip_version_only.want_ipv4()).await?;
    let hop = PortHop::new(
        state.egress_codec.clone(),
        state.egress_fec,
        state.egress.kcp_mtu,
    );
    Ok(hop.probe(&session.egress, &candidates).await?)
}
