// SPDX-License-Identifier: GPL-3.0-only

//! 端口跳跃探测（对应 C++ `test_before_change` / `change_new_port`）。
//!
//! 各 mode 的差异只在「何时探测」与「目标怎么选」，探测本身完全相同：
//! 新建一条 conv 0 的临时通道 → 发 `test_connection` → 3 秒内收到响应即成功 →
//! 把主通道重绑到新 socket 与目标地址，旧 socket 交还调用方继续收尾。
//!
//! 这里只保留与 IO 相关的部分；定时、目标选择与日志留在各 mode。

use crate::channel::KcpChannel;
use crate::udp::{UdpEndpoint, wildcard_addr};
use crate::{MAX_INNER_MESSAGE, PacketCodec, TransportError, now_ms};
use kcptube_kcp::{KcpConfig, KcpError};
use kcptube_protocol::constant::PACKET_LENGTH;
use kcptube_protocol::packet::{Feature, create_test_connection_packet, unpack_inner};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::time::{Duration, Instant, interval};

/// 探测超时（C++ `test_before_change` 固定 3 秒）。
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// 端口跳跃探测所需的共享参数。
pub struct PortHop {
    codec: Arc<PacketCodec>,
    fec: Option<(u8, u8)>,
    kcp_mtu: i32,
    timeout: Duration,
}

impl PortHop {
    pub fn new(codec: Arc<PacketCodec>, fec: Option<(u8, u8)>, kcp_mtu: i32) -> Self {
        Self {
            codec,
            fec,
            kcp_mtu,
            timeout: PROBE_TIMEOUT,
        }
    }

    /// 覆盖探测超时（默认 3 秒）。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 依次探测候选地址，第一个成功的地址会用于重绑 `channel`，返回旧 socket。
    ///
    /// 单个候选的绑定失败或超时只算失败，继续下一个；协议/驱动错误会中断。
    pub async fn probe(
        &self,
        channel: &Arc<KcpChannel>,
        peers: &[SocketAddr],
    ) -> Result<Option<UdpEndpoint>, TransportError> {
        for peer in peers {
            if let Some(old) = self.probe_one(channel, *peer).await? {
                return Ok(Some(old));
            }
        }
        Ok(None)
    }

    async fn probe_one(
        &self,
        channel: &Arc<KcpChannel>,
        peer: SocketAddr,
    ) -> Result<Option<UdpEndpoint>, TransportError> {
        let socket = match UdpEndpoint::bind(wildcard_addr(peer)).await {
            Ok(socket) => socket,
            Err(error) => {
                tracing::trace!("端口跳跃无法绑定本地 socket（{peer}）：{error}");
                return Ok(None);
            }
        };

        let probe = KcpChannel::new_with_fec(
            self.codec.clone(),
            KcpConfig::handshake(self.kcp_mtu),
            socket.clone(),
            peer,
            self.fec,
        )
        .await?;
        // 探测通道只需要 blast 与最小重传超时，不参与带宽限制（与 C++ 一致）
        probe.set_blast(true);
        probe.set_rx_minrto(10).await;
        probe.send(&create_test_connection_packet()).await?;
        probe.update(now_ms()).await?;

        let deadline = Instant::now() + self.timeout;
        let mut ticker = interval(Duration::from_millis(5));
        let mut datagram = vec![0u8; PACKET_LENGTH + 256];
        let mut inner = vec![0u8; MAX_INNER_MESSAGE];

        loop {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::select! {
                _ = ticker.tick() => {
                    probe.update(now_ms()).await?;
                }
                result = socket.recv_from(&mut datagram) => {
                    if let Ok((size, _)) = result {
                        let _ = probe.process_datagram(&datagram[..size]).await;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => return Ok(None),
            }

            loop {
                match probe.receive(&mut inner).await {
                    Ok(size) => {
                        let (feature, _, _) = unpack_inner(&inner[..size])?;
                        // `test_connection` 与 `keep_alive` 共用同一 feature 值
                        if matches!(feature, Feature::KeepAlive | Feature::KeepAliveResponse) {
                            let old = channel.rebind(socket.clone(), peer)?;
                            return Ok(Some(old));
                        }
                    }
                    Err(TransportError::Kcp(KcpError::WouldBlock)) => break,
                    Err(error) => return Err(error),
                }
            }
        }
    }
}
