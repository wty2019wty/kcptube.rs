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
pub mod fec;
pub mod mux;
pub mod tcp;
pub mod udp;

pub use channel::KcpChannel;
pub use fec::{FecReceiver, FecSender};
pub use mux::{MuxCommand, MuxEvent, MuxRecord, MuxSink, MuxTcpSink, MuxTunnel};
pub use tcp::{TcpListenerSession, TcpSession};
pub use udp::{UdpBindOptions, UdpEndpoint, wildcard_addr};

use kcptube_crypto::{Cipher, CryptoError, EncryptionMode};
use kcptube_kcp::{Kcp, KcpConfig, KcpError};
use kcptube_protocol::constant;
use kcptube_protocol::packet::{PacketLayerData, PacketLayerFec};
use kcptube_protocol::{PacketLayer, ProtocolError};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::net::SocketAddr;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

/// 单条 KCP 消息的最大长度（KCP 会把分片重组成一条消息，可能远大于单个 UDP 包）。
pub const MAX_INNER_MESSAGE: usize = 65536;

/// Windows 上向不可达的 UDP 目标发包时，ICMP 端口不可达会以
/// `ConnectionReset` / `ConnectionAborted`（WSAECONNRESET）的形式回传给下一次 recv。
///
/// 对应 C++ `udp_client::handle_receive` 里对 `error` 的「重新挂起接收」处理：
/// 这类错误不代表链路本身有问题，只应记 trace 并继续。
pub fn is_connection_reset(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
    )
}

/// 单个 UDP 数据报的加解密与包层处理（对应 C++ `encrypt_data` + `create_packet`）。///
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
        let (timestamp, plain) = self.open_frame(datagram)?;
        Ok((timestamp, plain[PacketLayer::HEADER_SIZE..].to_vec()))
    }

    /// 加密一个已经拼好包头的明文（FEC 包头由上层构造）。
    pub fn seal_frame(&self, plain: &[u8]) -> Result<Vec<u8>, TransportError> {
        Ok(self.cipher.encrypt(plain)?)
    }

    /// 解密并做时间窗校验，返回**完整明文**（含包头，供 FEC 层自行解析）。
    ///
    /// `packet_layer` 与 `packet_layer_data` / `packet_layer_fec` 的前 4 字节
    /// 都是小端时间戳，因此这里用 `packet_layer` 解析时间戳对三种帧都成立。
    pub fn open_frame(&self, datagram: &[u8]) -> Result<(u32, Vec<u8>), TransportError> {
        let plain = self.cipher.decrypt(datagram)?;
        let (header, _) = PacketLayer::decode(&plain)?;

        // 对应 C++ `calculate_difference<int64_t>((uint32_t)now, packet_timestamp)`。
        //
        // C++ 的 `calculate_difference` 是 `std::abs(a - b)`，**双向**容忍
        // ±`gbv_time_gap_seconds` 秒的时钟偏差。这里早期误写成单向无符号减法：
        // 只要对端时钟比本机快 1 秒，差值就会回绕成 40 亿而被全部丢弃，
        // 表现为两台设备（时钟不同步）完全连不上。必须与 C++ 一样取绝对值。
        let difference = (i64::from(now_seconds_u32()) - i64::from(header.timestamp)).abs();
        if difference > constant::TIME_GAP_SECONDS {
            return Err(TransportError::TimeGap);
        }

        Ok((header.timestamp, plain))
    }
}

fn now_seconds_u32() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as u32)
        .unwrap_or(0)
}

/// 解密后的帧类型。
///
/// - 未启用 FEC：`packet_layer` + KCP 段
/// - 启用 FEC：原始包用 `packet_layer_data`，冗余包用 `packet_layer_fec`
#[derive(Debug, Clone, Copy)]
pub enum IncomingFrame<'a> {
    /// 普通帧：负载即 KCP 段
    Plain(&'a [u8]),
    /// FEC 原始包（`sub_sn < k`）
    FecOriginal {
        sn: u32,
        sub_sn: u8,
        segment: &'a [u8],
    },
    /// FEC 冗余包（`sub_sn >= k`），`conv` 来自 FEC 头
    FecRedundant {
        sn: u32,
        sub_sn: u8,
        conv: u32,
        share: &'a [u8],
    },
}

impl IncomingFrame<'_> {
    /// 该帧对应的 KCP conv（用于服务端路由）。
    pub fn conv(&self) -> Option<u32> {
        match self {
            IncomingFrame::Plain(segment) | IncomingFrame::FecOriginal { segment, .. } => {
                Kcp::peek_conv(segment)
            }
            IncomingFrame::FecRedundant { conv, .. } => Some(*conv),
        }
    }
}

/// 解析解密后的明文帧。
///
/// `fec_original` 为 `Some(k)` 表示启用了 FEC（`k` 是原始包数量）。
pub fn parse_frame(
    plain: &[u8],
    fec_original: Option<usize>,
) -> Result<IncomingFrame<'_>, TransportError> {
    match fec_original {
        None => {
            let (_, payload) = PacketLayer::decode(plain)?;
            Ok(IncomingFrame::Plain(payload))
        }
        Some(k) => {
            let (header, payload) = PacketLayerData::decode(plain)?;
            if (header.sub_sn as usize) < k {
                Ok(IncomingFrame::FecOriginal {
                    sn: header.sn,
                    sub_sn: header.sub_sn,
                    segment: payload,
                })
            } else {
                let (header, share) = PacketLayerFec::decode(plain)?;
                Ok(IncomingFrame::FecRedundant {
                    sn: header.sn,
                    sub_sn: header.sub_sn,
                    conv: header.kcp_conv,
                    share,
                })
            }
        }
    }
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
    #[error("FEC 错误：{0}")]
    Fec(#[from] kcptube_fec::FecError),
    #[error("内部锁已中毒")]
    LockPoisoned,
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
    resolve_endpoint_filtered(host, port, None).await
}

/// 按地址族解析 `host:port`，返回**全部**候选地址（保持解析顺序、去重）。
///
/// C++ 版用 `udp::resolver` 的 `v4_mapped | all_matching` 解析，
/// 通过一个双栈 socket 同时覆盖 IPv4 / IPv6。Rust 的 socket 有明确地址族，
/// 因此这里保留所有候选，让上层在某个地址不可达时继续尝试下一个
/// （例如 DDNS 同时有 A/AAAA 记录，而服务端只监听其中一种）。
pub async fn resolve_endpoints_filtered(
    host: &str,
    port: u16,
    want_ipv4: Option<bool>,
) -> std::io::Result<Vec<SocketAddr>> {
    let matches = |ip: &std::net::IpAddr| match want_ipv4 {
        Some(true) => ip.is_ipv4(),
        Some(false) => ip.is_ipv6(),
        None => true,
    };

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if !matches(&ip) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                format!("地址 {host} 不符合配置的 IP 版本限制"),
            ));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let mut addresses = Vec::new();
    for address in tokio::net::lookup_host((host, port)).await? {
        if matches(&address.ip()) && !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    if addresses.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            format!("无法解析地址 {host}:{port}"),
        ));
    }
    Ok(addresses)
}

/// 按地址族解析 `host:port`。
///
/// `want_ipv4` 为 `Some(true)` / `Some(false)` 时只接受 IPv4 / IPv6 地址，
/// 对应配置里的 `ipv4_only` / `ipv6_only`。
pub async fn resolve_endpoint_filtered(
    host: &str,
    port: u16,
    want_ipv4: Option<bool>,
) -> std::io::Result<SocketAddr> {
    resolve_endpoints_filtered(host, port, want_ipv4)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| {
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
    fn wildcard_matches_peer_family() {
        let v4: SocketAddr = "1.2.3.4:80".parse().expect("地址");
        assert_eq!(wildcard_addr(v4), "0.0.0.0:0".parse().expect("地址"));
        let v6: SocketAddr = "[::1]:80".parse().expect("地址");
        assert_eq!(wildcard_addr(v6), "[::]:0".parse().expect("地址"));
    }

    #[test]
    fn connection_reset_kinds() {
        let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        let aborted = std::io::Error::from(std::io::ErrorKind::ConnectionAborted);
        let timed_out = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert!(is_connection_reset(&reset));
        assert!(is_connection_reset(&aborted));
        assert!(!is_connection_reset(&timed_out));
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

    #[tokio::test]
    async fn resolve_respects_ip_family() {
        let v4 = resolve_endpoint_filtered("127.0.0.1", 80, Some(true))
            .await
            .unwrap();
        assert!(v4.is_ipv4());
        assert!(
            resolve_endpoint_filtered("127.0.0.1", 80, Some(false))
                .await
                .is_err()
        );
        assert!(
            resolve_endpoint_filtered("::1", 80, Some(false))
                .await
                .is_ok()
        );
        assert!(
            resolve_endpoint_filtered("::1", 80, Some(true))
                .await
                .is_err()
        );
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

        // 超过 255 秒的偏差（过去或未来）都要丢弃，与 C++ `std::abs` 一致
        let stale = codec.seal_at(now.wrapping_sub(300), b"x").unwrap();
        assert!(matches!(codec.open(&stale), Err(TransportError::TimeGap)));

        let too_far_future = codec.seal_at(now.wrapping_add(300), b"x").unwrap();
        assert!(matches!(
            codec.open(&too_far_future),
            Err(TransportError::TimeGap)
        ));

        let fresh = codec.seal_at(now.wrapping_sub(100), b"x").unwrap();
        assert!(codec.open(&fresh).is_ok());

        // 对端时钟略微超前（两台设备未严格同步的常态）必须照常接受。
        // 早期单向无符号减法会把它当成 40 亿秒的偏差，导致跨设备完全连不上。
        let slightly_ahead = codec.seal_at(now.wrapping_add(1), b"x").unwrap();
        assert!(codec.open(&slightly_ahead).is_ok());

        let ahead = codec.seal_at(now.wrapping_add(120), b"x").unwrap();
        assert!(codec.open(&ahead).is_ok());
    }
}
