// SPDX-License-Identifier: GPL-3.0-only

//! 网络传输层。
//!
//! 对应 C++ 版 `networks/` 目录：`connections.cpp`（asio 封装）、
//! `kcp_updater.cpp`（KCP 定时驱动）、`mux_tunnel.cpp`（多路复用）。
//!
//! ## 与 asio 的映射
//!
//! | C++ | Rust |
//! |---|---|
//! | `asio::io_context`（light / heavy 两个） | 两个 `tokio` runtime，或一个多线程 runtime + 任务分组 |
//! | `asio::steady_timer` | `tokio::time::sleep` / `interval` |
//! | `shared_ptr<tcp_session>` 生命周期 | `Arc<Session>` + 显式关闭信号 |
//! | `asio::post` 跨线程投递 | `mpsc` / `JoinSet` |
//! | `KCPUpdater` 独立线程 + 条件变量 | 本文件的最小堆调度器 + 调用方 `select!` |

pub mod channel;
pub mod tcp;
pub mod udp;

pub use channel::KcpChannel;
pub use tcp::{TcpListenerSession, TcpSession};
pub use udp::{UdpBindOptions, UdpEndpoint};

use kcptube_crypto::{Cipher, CryptoError, EncryptionMode};
use kcptube_kcp::{Kcp, KcpConfig, KcpError};
use kcptube_protocol::constant;
use kcptube_protocol::{PacketLayer, ProtocolError};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::net::SocketAddr;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

/// 单个 UDP 数据报的加解密与包层处理（对应 C++ `encrypt_data` + `create_packet`）。
///
/// 顺序：`data_layer 负载` → 加 `packet_layer` 时间戳 → 加密（尾部带 `iv_raw`）。
pub struct PacketCodec {
    cipher: Cipher,
}

impl PacketCodec {
    pub fn new(mode: EncryptionMode, password: &str) -> Self {
        Self {
            cipher: Cipher::new(mode, password),
        }
    }

    pub fn mode(&self) -> EncryptionMode {
        self.cipher.mode()
    }

    /// 用指定时间戳封装（便于单元测试）
    pub fn seal_at(&self, timestamp: u32, inner: &[u8]) -> Result<Vec<u8>, TransportError> {
        let packet = PacketLayer::encode(timestamp, inner);
        Ok(self.cipher.encrypt(&packet)?)
    }

    /// 封装并使用当前 Unix 秒（低 32 位）
    pub fn seal(&self, inner: &[u8]) -> Result<Vec<u8>, TransportError> {
        self.seal_at(now_seconds_u32(), inner)
    }

    /// 解密并拆包，返回 `packet_layer` 之后的数据。
    pub fn open(&self, datagram: &[u8]) -> Result<(u32, Vec<u8>), TransportError> {
        let plain = self.cipher.decrypt(datagram)?;
        let (header, payload) = PacketLayer::decode(&plain)?;

        // 对应 C++ `calculate_difference<int64_t>((uint32_t)now, packet_timestamp)`
        let difference = now_seconds_u32().wrapping_sub(header.timestamp);
        if i64::from(difference) > constant::TIME_GAP_SECONDS {
            return Err(TransportError::TimeGap);
        }

        Ok((header.timestamp, payload.to_vec()))
    }
}

fn now_seconds_u32() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as u32)
        .unwrap_or(0)
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("会话已关闭")]
    Closed,
    #[error("加密层错误：{0}")]
    Crypto(#[from] CryptoError),
    #[error("KCP 错误：{0}")]
    Kcp(#[from] KcpError),
    #[error("协议层错误：{0}")]
    Protocol(#[from] ProtocolError),
    #[error("时间戳超出允许范围")]
    TimeGap,
    #[error("功能尚未实现：{0}")]
    NotImplemented(&'static str),
}

/// KCP 会话 + 对端地址（对应 C++ `kcp_mappings`）
pub struct KcpEndpoint {
    pub kcp: Mutex<Kcp>,
    pub peer: SocketAddr,
}

impl KcpEndpoint {
    pub fn new(config: KcpConfig, peer: SocketAddr) -> Self {
        Self {
            kcp: Mutex::new(Kcp::new(config)),
            peer,
        }
    }
}

/// 调度项：`at` 为 `Kcp::check` 返回的毫秒时间戳。
struct Scheduled {
    at: u32,
    sequence: u64,
    endpoint: Arc<KcpEndpoint>,
}

impl PartialEq for Scheduled {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at && self.sequence == other.sequence
    }
}

impl Eq for Scheduled {}

impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap 是最大堆，这里反向比较，使最早到期的在堆顶
        other
            .at
            .cmp(&self.at)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

/// KCP 定时驱动（对应 C++ `KCPUpdater`）。
///
/// C++ 版是「独立线程 + 条件变量 + `std::map<time, kcp>`」；
/// Rust 版改为纯调度器，不自己持有任务：
///
/// ```ignore
/// loop {
///     let Some(deadline) = updater.next_deadline() else { ... };
///     tokio::select! {
///         _ = tokio::time::sleep_until(deadline) => updater.run_due(now_ms()).await,
///         _ = new_connection.recv() => updater.schedule(endpoint, now_ms()).await,
///     }
/// }
/// ```
pub struct KcpUpdater {
    heap: BinaryHeap<Scheduled>,
    sequence: u64,
}

impl KcpUpdater {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            sequence: 0,
        }
    }

    /// 注册 / 重新注册一个 KCP 会话，按 `kcp.check(now)` 安排下一次驱动。
    pub async fn schedule(&mut self, endpoint: Arc<KcpEndpoint>, now_ms: u32) {
        let next = {
            let kcp = endpoint.kcp.lock().await;
            kcp.check(now_ms)
        };
        self.push(endpoint, next);
    }

    /// 取出所有到期会话并驱动一次，返回本次驱动的数量。
    pub async fn run_due(&mut self, now_ms: u32) -> usize {
        let mut count = 0;
        while let Some(top) = self.heap.peek() {
            if !is_due(now_ms, top.at) {
                break;
            }
            let Some(entry) = self.heap.pop() else {
                break;
            };

            let next = {
                let mut kcp = entry.endpoint.kcp.lock().await;
                kcp.update(now_ms);
                kcp.check(now_ms)
            };
            // 防御：没有 output 回调或极端状态下 check 仍返回“现在”，
            // 推迟 1ms，避免同一轮 run_due 里自旋。
            let next = if is_due(now_ms, next) {
                now_ms.wrapping_add(1)
            } else {
                next
            };
            self.push(entry.endpoint, next);
            count += 1;
        }
        count
    }

    /// 最近一次需要驱动的时刻；没有会话时为 `None`
    pub fn next_deadline(&self) -> Option<u32> {
        self.heap.peek().map(|entry| entry.at)
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    fn push(&mut self, endpoint: Arc<KcpEndpoint>, at: u32) {
        self.sequence = self.sequence.wrapping_add(1);
        self.heap.push(Scheduled {
            at,
            sequence: self.sequence,
            endpoint,
        });
    }
}

impl Default for KcpUpdater {
    fn default() -> Self {
        Self::new()
    }
}

/// 判断 `now` 是否已到达 `at`（考虑 u32 回绕）。
fn is_due(now: u32, at: u32) -> bool {
    (now.wrapping_sub(at) as i32) >= 0
}

/// 解析 `host:port`（支持域名），返回首个地址。
pub async fn resolve_endpoint(host: &str, port: u16) -> std::io::Result<SocketAddr> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let mut addresses = tokio::net::lookup_host((host, port)).await?;
    addresses.next().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("无法解析地址 {host}:{port}"),
        )
    })
}

/// 当前系统时间（毫秒，取低 32 位；对应 C++ `TimeNowForKCP`）
pub fn now_ms() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u32)
        .unwrap_or(0)
}

/// 当前系统时间（Unix 秒；对应 C++ `packet::right_now()`）
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn endpoint() -> Arc<KcpEndpoint> {
        Arc::new(KcpEndpoint::new(
            KcpConfig::default(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 30000),
        ))
    }

    #[test]
    fn updater_can_be_created() {
        let updater = KcpUpdater::new();
        assert!(updater.is_empty());
        assert_eq!(updater.next_deadline(), None);
    }

    #[test]
    fn due_handles_u32_wrap() {
        assert!(is_due(1000, 1000));
        assert!(is_due(1001, 1000));
        assert!(!is_due(999, 1000));
        assert!(is_due(1, u32::MAX));
    }

    #[tokio::test]
    async fn schedules_and_runs_due_sessions() {
        let mut updater = KcpUpdater::new();
        let endpoint = endpoint();
        updater.schedule(endpoint.clone(), 1000).await;
        assert_eq!(updater.len(), 1);

        // 尚未 update 时 check 返回 current；驱动一次后才是 current + interval
        let deadline = updater.next_deadline().expect("应有调度项");
        assert_eq!(deadline, 1000);

        assert_eq!(updater.run_due(1000).await, 1);
        assert_eq!(updater.next_deadline(), Some(1010));
        assert_eq!(updater.run_due(1000).await, 0);
        assert_eq!(updater.run_due(1010).await, 1);
        assert!(updater.next_deadline().is_some());
    }

    #[tokio::test]
    async fn multiple_sessions_ordered_by_deadline() {
        let mut updater = KcpUpdater::new();
        let first = endpoint();
        let second = endpoint();

        {
            let mut kcp = first.kcp.lock().await;
            kcp.update(1000);
        }
        {
            let mut kcp = second.kcp.lock().await;
            kcp.update(2000);
        }

        updater.schedule(first, 1000).await; // deadline 1010
        updater.schedule(second, 2000).await; // deadline 2010
        assert_eq!(updater.next_deadline(), Some(1010));
        assert_eq!(updater.run_due(1010).await, 1);
        assert_eq!(updater.next_deadline(), Some(1020));
        assert_eq!(updater.run_due(2000).await, 1);
        assert_eq!(updater.next_deadline(), Some(2010));
        assert_eq!(updater.run_due(2010).await, 2);
    }

    #[test]
    fn packet_codec_roundtrip() {
        for mode in [
            EncryptionMode::None,
            EncryptionMode::PlainXor,
            EncryptionMode::AesGcm,
            EncryptionMode::Chacha20,
        ] {
            let codec = PacketCodec::new(mode, "test-password");
            let datagram = codec.seal(b"kcp segment here").unwrap();
            let (_, payload) = codec.open(&datagram).unwrap();
            assert_eq!(payload, b"kcp segment here", "{mode:?}");
        }
    }

    #[test]
    fn packet_codec_rejects_tampering() {
        let codec = PacketCodec::new(EncryptionMode::AesGcm, "test-password");
        let mut datagram = codec.seal(b"hello").unwrap();
        let last = datagram.len() - 1;
        datagram[last] ^= 0xFF;
        assert!(codec.open(&datagram).is_err());
    }

    #[test]
    fn packet_codec_rejects_stale_and_future_timestamps() {
        let codec = PacketCodec::new(EncryptionMode::None, "");
        let now = now_seconds_u32();

        let stale = codec.seal_at(now.wrapping_sub(300), b"x").unwrap();
        assert!(matches!(codec.open(&stale), Err(TransportError::TimeGap)));

        let fresh = codec.seal_at(now.wrapping_sub(100), b"x").unwrap();
        assert!(codec.open(&fresh).is_ok());

        // C++ 使用无符号差值，未来时间戳会被视为超大差值而丢弃
        let future = codec.seal_at(now.wrapping_add(1), b"x").unwrap();
        assert!(matches!(codec.open(&future), Err(TransportError::TimeGap)));
    }
}
