// SPDX-License-Identifier: GPL-3.0-only

//! 数据会话：两条 KCP 通道之间的透明转发（对应 C++
//! `udp_listener_incoming` 的数据分支与 `udp_forwarder_incoming`）。
//!
//! 每条会话独占一个任务：
//! - listener 把 ingress 方向的明文帧投递到会话队列；
//! - 会话任务自己收 egress socket；
//! - 每 5ms 驱动两条 KCP 并各自 drain。
//!
//! 转发的是**完整内层数据层**（含 `data_layer` 头），与 C++
//! `kcp_ptr->Send(buffer, kcp_data_size)` 一致。

use crate::RelayError;
use crate::listener::is_drained;
use crate::state::{RelaySession, RelayState};
use kcptube_protocol::constant::{KCP_CLEANUP_WAITS, PACKET_LENGTH};
use kcptube_protocol::packet::{Feature, create_keep_alive_response_packet, unpack_inner};
use kcptube_transport::{MAX_INNER_MESSAGE, now_ms, now_secs, parse_frame};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, interval};

/// 会话任务：驱动两侧 KCP、转发数据，直到断开。
pub(crate) async fn run_session(
    state: Arc<RelayState>,
    session: Arc<RelaySession>,
    mut rx: mpsc::Receiver<Vec<u8>>,
) {
    let mut ticker = interval(Duration::from_millis(5));
    let mut datagram = vec![0u8; PACKET_LENGTH + 256];
    let mut inner = vec![0u8; MAX_INNER_MESSAGE];

    loop {
        if session.is_closed() {
            break;
        }
        tokio::select! {
            received = rx.recv() => {
                let Some(plain) = received else { break };
                let Ok(frame) = parse_frame(&plain, state.ingress_fec_original) else {
                    continue;
                };
                if session.ingress.accept_frame(frame).await.is_err() {
                    continue;
                }
                let _ = drain(&session, true, &mut inner).await;
            }
            result = session.egress_socket.recv_from(&mut datagram) => {
                let Ok((size, _)) = result else { continue };
                if session.egress.process_datagram(&datagram[..size]).await.is_err() {
                    continue;
                }
                let _ = drain(&session, false, &mut inner).await;
            }
            _ = ticker.tick() => {
                let now = now_ms();
                let _ = session.ingress.update(now).await;
                let _ = session.egress.update(now).await;
                let _ = drain(&session, true, &mut inner).await;
                let _ = drain(&session, false, &mut inner).await;
            }
        }
    }

    state.sessions.lock().await.remove(&session.conv);

    // 保留两侧通道最多 `KCP_CLEANUP_WAITS` 秒，冲刷最后的 disconnect 重传
    let deadline = Instant::now() + Duration::from_secs(KCP_CLEANUP_WAITS as u64);
    while Instant::now() < deadline {
        let _ = session.ingress.update(now_ms()).await;
        let _ = session.egress.update(now_ms()).await;
        if session.ingress.waiting_for_send().await == 0
            && session.egress.waiting_for_send().await == 0
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 把一侧 KCP 里已就绪的内层帧全部取出，按 feature 转发到另一侧。
///
/// `from_ingress = true` 表示方向为「客户端 → relay → 服务端」。
async fn drain(
    session: &Arc<RelaySession>,
    from_ingress: bool,
    buffer: &mut [u8],
) -> Result<(), RelayError> {
    let (from, to) = if from_ingress {
        (&session.ingress, &session.egress)
    } else {
        (&session.egress, &session.ingress)
    };

    loop {
        let size = match from.receive(buffer).await {
            Ok(size) => size,
            Err(error) if is_drained(&error) => break,
            Err(error) => return Err(error.into()),
        };
        let (feature, protocol, _) = unpack_inner(&buffer[..size])?;
        match feature {
            Feature::KeepAlive => {
                // 保活只在本侧回应，不转发（对应 C++ 两侧同名分支）
                from.send(&create_keep_alive_response_packet(protocol))
                    .await?;
            }
            Feature::KeepAliveResponse => {
                from.mark_keep_alive_response(now_secs());
            }
            Feature::Failure | Feature::Disconnect => {
                to.send(&buffer[..size]).await?;
                session.close();
                break;
            }
            // raw_data / mux_transfer / mux_cancel / pre_connect_custom_address
            // 以及其他内层帧：原样转发
            _ => {
                to.send(&buffer[..size]).await?;
            }
        }
    }
    Ok(())
}
