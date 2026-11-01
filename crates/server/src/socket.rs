// SPDX-License-Identifier: GPL-3.0-only

//! 单个监听 socket 的收包循环与全局 KCP 驱动任务。

use crate::ServerError;
use crate::state::SocketState;
use crate::util::is_connection_reset;
use kcptube_protocol::constant::PACKET_LENGTH;
use kcptube_transport::{UdpEndpoint, parse_frame};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::time::{Duration, interval};

impl SocketState {
    /// 单个监听 socket 的收包循环；所有端口共享同一个 `SocketState`。
    pub(crate) async fn run_socket(
        self: Arc<Self>,
        socket: UdpEndpoint,
    ) -> Result<(), ServerError> {
        let listener_addr = socket.local_addr()?;
        let mut buffer = vec![0u8; PACKET_LENGTH + 256];

        loop {
            match socket.recv_from(&mut buffer).await {
                Ok((size, peer)) => {
                    if let Err(error) = self
                        .handle_datagram(&socket, listener_addr, peer, &buffer[..size])
                        .await
                    {
                        tracing::debug!("丢弃来自 {peer} 的数据报：{error}");
                    }
                }
                Err(error) if is_connection_reset(&error) => {
                    // Windows 上 UDP 收到 ICMP 端口不可达（对端已关闭）时会返回
                    // WSAECONNRESET；会话关闭后仍在重传尾包时很常见，忽略即可。
                    tracing::trace!("UDP 收到对端不可达提示，已忽略");
                }
                Err(error) => {
                    tracing::warn!("UDP 接收失败：{error}");
                }
            }
        }
    }

    /// KCP 定时驱动与维护：全局只启动一份，避免多个监听 socket 重复 update。
    pub(crate) async fn run_driver(self: Arc<Self>) -> Result<(), ServerError> {
        // KCP 驱动：5ms 一次，与 C++ `KCPUpdater` 的调度精度一致
        let mut kcp_ticker = interval(Duration::from_millis(5));
        // 维护：每秒一次（对应 C++ `gbv_expring_update_interval`）
        let mut maintenance = interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                _ = kcp_ticker.tick() => {
                    self.tick_kcp().await;
                }
                _ = maintenance.tick() => {
                    self.maintain().await;
                    self.maybe_log_status().await;
                }
            }
        }
    }

    async fn handle_datagram(
        self: &Arc<Self>,
        socket: &UdpEndpoint,
        listener_addr: SocketAddr,
        peer: SocketAddr,
        datagram: &[u8],
    ) -> Result<(), ServerError> {
        let (_, plain) = self.codec.open_frame(datagram)?;
        let frame = parse_frame(&plain, self.fec_original)?;
        let Some(conv) = frame.conv() else {
            return Ok(());
        };
        self.counters.add_ingress_raw(datagram.len());
        self.counters.add_ingress_inner(plain.len());

        if conv == 0 {
            self.handle_handshake(socket, peer, frame).await
        } else {
            self.handle_data(socket, listener_addr, conv, peer, frame)
                .await
        }
    }
}
