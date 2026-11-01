// SPDX-License-Identifier: GPL-3.0-only

//! KCP 通道：把 `Kcp`、`PacketCodec` 与 `UdpEndpoint` 串成完整收发流水线。
//!
//! 发送：上层数据 → `Kcp::send` → KCP 段 → `PacketCodec::seal`
//!       （加时间戳 + 加密 + `iv_raw`）→ `UdpEndpoint::try_send_to`
//!
//! 接收：UDP 数据报 → `PacketCodec::open`（解密 + 拆包 + 时间窗）→ `Kcp::input`

use crate::fec::{FecReceiver, FecSender};
use crate::udp::UdpEndpoint;
use crate::{
    IncomingFrame, KcpEndpoint, PacketCodec, TransportError, now_ms, now_secs, parse_frame,
};
use kcptube_kcp::{KcpConfig, KcpError};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

/// C++ 各 mode 统一使用的最小重传超时（`RxMinRTO = 10` 毫秒）。
const RX_MIN_RTO: i32 = 10;

/// 建立一条面向 `peer` 的**独占 socket** 握手通道（对应 C++ `create_handshake`）。
///
/// 按对端地址族绑定本地通配端口，使用固定握手参数；调用方随后应执行
/// [`KcpChannel::apply_handshake_tuning`]。服务端共享监听 socket 的场景不适用，
/// 那种情况直接用 [`KcpChannel::new_with_fec`]。
pub async fn open_handshake_channel(
    codec: Arc<PacketCodec>,
    kcp_mtu: i32,
    peer: SocketAddr,
    fec: Option<(u8, u8)>,
) -> Result<(KcpChannel, UdpEndpoint), TransportError> {
    let socket = UdpEndpoint::bind(crate::udp::wildcard_addr(peer)).await?;
    let channel = KcpChannel::new_with_fec(
        codec,
        KcpConfig::handshake(kcp_mtu),
        socket.clone(),
        peer,
        fec,
    )
    .await?;
    Ok((channel, socket))
}

/// 一条完整的 KCP 数据通道。
///
/// 发送 socket 可以在运行期切换（端口跳跃），因此内部用共享单元保存；
/// 接收方应通过 [`KcpChannel::socket`] 每次取当前 socket 的克隆。
pub struct KcpChannel {
    pub codec: Arc<PacketCodec>,
    pub endpoint: Arc<KcpEndpoint>,
    socket: Arc<StdMutex<UdpEndpoint>>,
    socket_addr: Arc<StdMutex<SocketAddr>>,
    peer: Arc<std::sync::Mutex<SocketAddr>>,
    /// `blast`：发送后立即 flush（对应 C++ `Refresh()`）
    blast: AtomicBool,
    /// 最近一次发送 `keep_alive` 的 Unix 秒（对应 C++ `keep_alive_send_time`）
    keep_alive_send_time: AtomicI64,
    /// 最近一次收到 `keep_alive_response` 的 Unix 秒（对应 C++ `keep_alive_response_time`）
    keep_alive_response_time: AtomicI64,
    /// 已发出的加密数据报字节数（用于状态日志的 `send (raw)`）
    output_bytes: Arc<AtomicU64>,
    /// 已交给 KCP 的负载字节数（用于状态日志的 `receive (inner)`）
    input_bytes: Arc<AtomicU64>,
    /// FEC 接收侧；`None` 表示未启用
    fec_receiver: Option<StdMutex<FecReceiver>>,
    /// FEC 原始包数量（用于区分原始包与冗余包）
    fec_original: Option<usize>,
}

impl KcpChannel {
    /// 建立通道并注册 KCP 输出回调（不启用 FEC）。
    pub async fn new(
        codec: Arc<PacketCodec>,
        config: KcpConfig,
        socket: UdpEndpoint,
        peer: SocketAddr,
    ) -> Result<Self, TransportError> {
        Self::new_with_fec(codec, config, socket, peer, None).await
    }

    /// 建立通道；`fec` 为 `(原始包数 k, 冗余包数 r)`，`None` 表示关闭 FEC。
    pub async fn new_with_fec(
        codec: Arc<PacketCodec>,
        config: KcpConfig,
        socket: UdpEndpoint,
        peer: SocketAddr,
        fec: Option<(u8, u8)>,
    ) -> Result<Self, TransportError> {
        let conv = config.conv;
        let local_addr = socket.local_addr()?;
        let endpoint = Arc::new(KcpEndpoint::new(config, peer));
        let socket_cell = Arc::new(StdMutex::new(socket));
        let socket_addr = Arc::new(StdMutex::new(local_addr));
        let peer_cell = Arc::new(std::sync::Mutex::new(peer));
        let output_bytes = Arc::new(AtomicU64::new(0));

        let fec_sender = match fec {
            Some((original, redundant)) => FecSender::new(original, redundant, conv)?
                .map(|sender| Arc::new(StdMutex::new(sender))),
            None => None,
        };
        let fec_receiver = match fec {
            Some((original, redundant)) => {
                FecReceiver::new(original, redundant)?.map(StdMutex::new)
            }
            None => None,
        };
        let fec_original = fec.map(|(original, _)| original as usize);

        {
            let codec = codec.clone();
            let socket_cell = socket_cell.clone();
            let peer_cell = peer_cell.clone();
            let output_bytes = output_bytes.clone();
            let fec_sender = fec_sender.clone();
            let mut kcp = endpoint.kcp.lock().await;
            kcp.set_output(Box::new(move |payload: &[u8]| {
                let target = peer_cell.lock().map(|guard| *guard).unwrap_or(peer);
                let socket = socket_cell
                    .lock()
                    .map_err(|_| KcpError::OutputFailed)?
                    .clone();

                let transmit = |datagram: Vec<u8>| -> Result<(), KcpError> {
                    output_bytes.fetch_add(datagram.len() as u64, Ordering::Relaxed);
                    socket
                        .try_send_to(&datagram, target)
                        .map(|_| ())
                        .map_err(|_| KcpError::OutputFailed)
                };

                match &fec_sender {
                    Some(sender) => {
                        let (original, redundant) = sender
                            .lock()
                            .map_err(|_| KcpError::OutputFailed)?
                            .wrap(payload);
                        for frame in std::iter::once::<Vec<u8>>(original).chain(redundant) {
                            let datagram = codec
                                .seal_frame(&frame)
                                .map_err(|_| KcpError::OutputFailed)?;
                            transmit(datagram)?;
                        }
                    }
                    None => {
                        let datagram = codec.seal(payload).map_err(|_| KcpError::OutputFailed)?;
                        transmit(datagram)?;
                    }
                }
                Ok(())
            }));
        }

        let now = now_secs();
        Ok(Self {
            codec,
            endpoint,
            socket: socket_cell,
            socket_addr,
            peer: peer_cell,
            blast: AtomicBool::new(false),
            keep_alive_send_time: AtomicI64::new(now),
            keep_alive_response_time: AtomicI64::new(now),
            output_bytes,
            input_bytes: Arc::new(AtomicU64::new(0)),
            fec_receiver,
            fec_original,
        })
    }

    /// 是否启用了 FEC
    pub fn fec_enabled(&self) -> bool {
        self.fec_original.is_some()
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
            .lock()
            .map(|guard| *guard)
            .unwrap_or(self.endpoint.peer)
    }

    /// 更新对端地址。
    ///
    /// C++ 客户端在握手成功后会切换到一个新的本地 UDP 端口，
    /// 服务端必须按收到数据包的源地址更新回包目标（对应
    /// `server.cpp` 里对 `ingress_source_endpoint` 的更新）。
    pub fn set_peer(&self, peer: SocketAddr) {
        if let Ok(mut guard) = self.peer.lock() {
            *guard = peer;
        }
    }

    /// 当前发送 socket 的克隆（端口跳跃后由上层重新获取）。
    pub fn socket(&self) -> UdpEndpoint {
        self.socket
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket_addr
            .lock()
            .map(|guard| *guard)
            .map_err(|_| std::io::Error::other("socket 地址锁已中毒"))
    }

    /// 设置 `blast`：为 `true` 时每次发送后立即 flush（对应 C++ `Refresh()`）。
    pub fn set_blast(&self, enabled: bool) {
        self.blast.store(enabled, Ordering::Relaxed);
    }

    /// 设置 KCP 带宽（对应 C++ `SetBandwidth`）。
    pub async fn set_bandwidth(&self, outbound: u64, inbound: u64) {
        let mut kcp = self.endpoint.kcp.lock().await;
        kcp.set_bandwidth(outbound, inbound);
    }

    /// 设置最小重传超时（C++ 各 mode 统一使用 `RxMinRTO = 10`）。
    pub async fn set_rx_minrto(&self, value: i32) {
        let mut kcp = self.endpoint.kcp.lock().await;
        kcp.set_rx_minrto(value);
    }

    /// 握手通道的统一起始参数（对应 C++ `create_handshake`）：
    /// `SetBlast` + `SetBandwidth` + `RxMinRTO = 10`。
    pub async fn apply_handshake_tuning(&self, blast: bool, outbound: u64, inbound: u64) {
        self.set_blast(blast);
        let mut kcp = self.endpoint.kcp.lock().await;
        kcp.set_bandwidth(outbound, inbound);
        kcp.set_rx_minrto(RX_MIN_RTO);
    }

    /// 数据通道的统一起始参数：
    /// 在握手参数之外，再按握手阶段测得的 SRTT 重置收发窗口并驱动一次。
    pub async fn apply_data_tuning(&self, blast: bool, outbound: u64, inbound: u64, srtt: i32) {
        self.set_blast(blast);
        let mut kcp = self.endpoint.kcp.lock().await;
        kcp.set_bandwidth(outbound, inbound);
        kcp.reset_window_values(srtt);
        kcp.set_rx_minrto(RX_MIN_RTO);
        kcp.update(now_ms());
    }

    /// 按握手阶段的 SRTT 重置收发窗口（对应 C++ `ResetWindowValues`）。
    pub async fn reset_window_values(&self, srtt: i32) {
        let mut kcp = self.endpoint.kcp.lock().await;
        kcp.reset_window_values(srtt);
    }

    /// 切换发送 socket 与对端，返回旧 socket。
    ///
    /// 用于端口跳跃：KCP 会话与 conv 保持不变，只把输出重定向到新 socket。
    pub fn rebind(
        &self,
        socket: UdpEndpoint,
        peer: SocketAddr,
    ) -> Result<UdpEndpoint, TransportError> {
        let local_addr = socket.local_addr()?;
        // 先更新对端，避免新 socket 已就绪但输出仍指向旧地址的瞬时窗口
        self.set_peer(peer);
        let old = {
            let mut guard = self
                .socket
                .lock()
                .map_err(|_| TransportError::LockPoisoned)?;
            std::mem::replace(&mut *guard, socket)
        };
        if let Ok(mut guard) = self.socket_addr.lock() {
            *guard = local_addr;
        }
        Ok(old)
    }

    /// 上层发送数据
    pub async fn send(&self, data: &[u8]) -> Result<(), TransportError> {
        let mut kcp = self.endpoint.kcp.lock().await;
        kcp.send(data)?;
        if self.blast.load(Ordering::Relaxed) {
            let now = now_ms();
            kcp.ensure_started(now);
            let _ = kcp.flush(now);
        }
        Ok(())
    }

    /// 上层接收一条消息
    pub async fn receive(&self, buffer: &mut [u8]) -> Result<usize, TransportError> {
        let mut kcp = self.endpoint.kcp.lock().await;
        Ok(kcp.receive(buffer)?)
    }

    /// 处理一个收到的 UDP 数据报
    pub async fn process_datagram(&self, datagram: &[u8]) -> Result<(), TransportError> {
        let (_, plain) = self.codec.open_frame(datagram)?;
        let frame = parse_frame(&plain, self.fec_original)?;
        self.accept_frame(frame).await
    }

    /// 处理一个已解密、已解析的帧（服务端先按 conv 路由再调用）。
    pub async fn accept_frame(&self, frame: IncomingFrame<'_>) -> Result<(), TransportError> {
        match frame {
            IncomingFrame::Plain(segment) => self.input_payload(segment).await,
            IncomingFrame::FecOriginal {
                sn,
                sub_sn,
                segment,
            } => {
                let recovered =
                    self.push_fec(|receiver| receiver.push_original(sn, sub_sn, segment))?;
                self.input_payload(segment).await?;
                for segment in recovered {
                    self.input_payload(&segment).await?;
                }
                Ok(())
            }
            IncomingFrame::FecRedundant {
                sn, sub_sn, share, ..
            } => {
                let recovered =
                    self.push_fec(|receiver| receiver.push_redundant(sn, sub_sn, share))?;
                for segment in recovered {
                    self.input_payload(&segment).await?;
                }
                Ok(())
            }
        }
    }

    fn push_fec(
        &self,
        action: impl FnOnce(&mut FecReceiver) -> Vec<Vec<u8>>,
    ) -> Result<Vec<Vec<u8>>, TransportError> {
        match &self.fec_receiver {
            Some(receiver) => {
                let mut receiver = receiver.lock().map_err(|_| TransportError::LockPoisoned)?;
                Ok(action(&mut receiver))
            }
            None => Ok(Vec::new()),
        }
    }

    /// 处理一个已经解密、拆包后的 KCP 负载
    pub async fn input_payload(&self, payload: &[u8]) -> Result<(), TransportError> {
        self.input_bytes
            .fetch_add(payload.len() as u64, Ordering::Relaxed);
        let mut kcp = self.endpoint.kcp.lock().await;
        Ok(kcp.input(payload)?)
    }

    /// 从 socket 收一个数据报并直接交给 KCP
    pub async fn recv_and_process(&self, buffer: &mut [u8]) -> Result<usize, TransportError> {
        let (size, _peer) = self.socket().recv_from(buffer).await?;
        self.process_datagram(&buffer[..size]).await?;
        Ok(size)
    }

    /// 驱动一次 KCP，返回下次需要驱动的时刻
    pub async fn update(&self, now_ms: u32) -> Result<u32, TransportError> {
        let mut kcp = self.endpoint.kcp.lock().await;
        kcp.update(now_ms);
        Ok(kcp.check(now_ms))
    }

    pub async fn waiting_for_send(&self) -> usize {
        let kcp = self.endpoint.kcp.lock().await;
        kcp.waiting_for_send()
    }

    pub async fn is_dead(&self) -> bool {
        let kcp = self.endpoint.kcp.lock().await;
        kcp.is_dead()
    }

    pub async fn peek_size(&self) -> i32 {
        let kcp = self.endpoint.kcp.lock().await;
        kcp.peek_size()
    }

    /// 最近一次收到数据的 Unix 秒（对应 C++ `KCP::LastInputTime()`）
    pub async fn last_input_time(&self) -> i64 {
        let kcp = self.endpoint.kcp.lock().await;
        kcp.last_input_time()
    }

    /// KCP 平滑 RTT（毫秒），用于状态日志
    pub async fn rx_srtt(&self) -> i32 {
        let kcp = self.endpoint.kcp.lock().await;
        kcp.rx_srtt()
    }

    pub fn keep_alive_send_time(&self) -> i64 {
        self.keep_alive_send_time.load(Ordering::Relaxed)
    }

    pub fn keep_alive_response_time(&self) -> i64 {
        self.keep_alive_response_time.load(Ordering::Relaxed)
    }

    pub fn mark_keep_alive_sent(&self, at: i64) {
        self.keep_alive_send_time.store(at, Ordering::Relaxed);
    }

    pub fn mark_keep_alive_response(&self, at: i64) {
        self.keep_alive_response_time.store(at, Ordering::Relaxed);
    }

    /// 取出并清零「已发出的加密字节数」
    pub fn take_output_bytes(&self) -> u64 {
        self.output_bytes.swap(0, Ordering::Relaxed)
    }

    /// 取出并清零「已收到的 KCP 负载字节数」
    pub fn take_input_bytes(&self) -> u64 {
        self.input_bytes.swap(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kcptube_crypto::EncryptionMode;
    use tokio::time::{Duration, timeout};

    fn config(conv: u32) -> KcpConfig {
        KcpConfig {
            conv,
            mtu: 1400,
            nodelay: 1,
            interval: 10,
            resend: 2,
            no_congestion_control: true,
            send_window: 32,
            receive_window: 128,
            stream_mode: false,
        }
    }

    #[tokio::test]
    async fn channels_exchange_encrypted_data() {
        let first_socket = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let second_socket = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let first_addr = first_socket.local_addr().unwrap();
        let second_addr = second_socket.local_addr().unwrap();

        let codec = Arc::new(PacketCodec::new(EncryptionMode::AesGcm, "test-password"));
        let first = KcpChannel::new(
            codec.clone(),
            config(0x1122_3344),
            first_socket,
            second_addr,
        )
        .await
        .unwrap();
        let second = KcpChannel::new(codec, config(0x1122_3344), second_socket, first_addr)
            .await
            .unwrap();

        first.send(b"hello kcp").await.unwrap();
        first.update(1000).await.unwrap();

        let mut datagram = vec![0u8; 2048];
        let (size, _) = timeout(
            Duration::from_secs(1),
            second.socket().recv_from(&mut datagram),
        )
        .await
        .expect("应收到加密数据报")
        .unwrap();
        second.process_datagram(&datagram[..size]).await.unwrap();

        let mut buffer = vec![0u8; 64];
        assert_eq!(second.receive(&mut buffer).await.unwrap(), 9);
        assert_eq!(&buffer[..9], b"hello kcp");

        // 回 ACK，验证发送端清空 snd_buf
        second.update(1000).await.unwrap();
        let (size, _) = timeout(
            Duration::from_secs(1),
            first.socket().recv_from(&mut datagram),
        )
        .await
        .expect("应收到 ACK")
        .unwrap();
        first.process_datagram(&datagram[..size]).await.unwrap();
        assert_eq!(first.waiting_for_send().await, 0);
    }

    #[tokio::test]
    async fn blast_flushes_without_update() {
        let first_socket = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let second_socket = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let first_addr = first_socket.local_addr().unwrap();
        let second_addr = second_socket.local_addr().unwrap();

        let codec = Arc::new(PacketCodec::new(EncryptionMode::AesGcm, "test-password"));
        let first = KcpChannel::new(
            codec.clone(),
            config(0x2233_4455),
            first_socket,
            second_addr,
        )
        .await
        .unwrap();
        let second = KcpChannel::new(codec, config(0x2233_4455), second_socket, first_addr)
            .await
            .unwrap();

        first.set_blast(true);
        first.send(b"hello blast").await.unwrap();

        let mut datagram = vec![0u8; 2048];
        let (size, _) = timeout(
            Duration::from_secs(1),
            second.socket().recv_from(&mut datagram),
        )
        .await
        .expect("blast=true 时发送应立即 flush")
        .unwrap();
        second.process_datagram(&datagram[..size]).await.unwrap();

        let mut buffer = vec![0u8; 32];
        assert_eq!(second.receive(&mut buffer).await.unwrap(), 11);
        assert_eq!(&buffer[..11], b"hello blast");
    }

    #[tokio::test]
    async fn bandwidth_resizes_windows() {
        let socket = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let channel = KcpChannel::new(
            Arc::new(PacketCodec::new(EncryptionMode::AesGcm, "pw")),
            config(1),
            socket,
            "127.0.0.1:9".parse().unwrap(),
        )
        .await
        .unwrap();

        channel.set_bandwidth(1_000_000, 2_000_000).await;
        channel.reset_window_values(100).await;
        let kcp = channel.endpoint.kcp.lock().await;
        assert!(kcp.send_window_size() > 32);
        assert!(kcp.receive_window_size() > 32);
    }

    #[tokio::test]
    async fn handshake_channel_binds_peer_family() {
        let peer: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let (channel, socket) = open_handshake_channel(
            Arc::new(PacketCodec::new(EncryptionMode::None, "")),
            1418,
            peer,
            None,
        )
        .await
        .unwrap();
        assert!(socket.local_addr().unwrap().is_ipv4());
        assert_eq!(channel.peer(), peer);
        assert_eq!(channel.local_addr().unwrap(), socket.local_addr().unwrap());
    }

    #[tokio::test]
    async fn tuning_helpers_configure_bandwidth_and_windows() {
        let socket = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let channel = KcpChannel::new(
            Arc::new(PacketCodec::new(EncryptionMode::AesGcm, "pw")),
            config(2),
            socket,
            "127.0.0.1:9".parse().unwrap(),
        )
        .await
        .unwrap();

        // 握手通道：只设带宽与最小重传超时，窗口算法留给数据通道
        channel
            .apply_handshake_tuning(true, 1_000_000, 2_000_000)
            .await;
        {
            let kcp = channel.endpoint.kcp.lock().await;
            assert_eq!(kcp.send_window_size(), 32);
            assert_eq!(kcp.receive_window_size(), 128);
        }

        // 数据通道：带宽 + 按 SRTT 重置窗口 + 驱动一次
        channel
            .apply_data_tuning(false, 1_000_000, 2_000_000, 100)
            .await;
        let kcp = channel.endpoint.kcp.lock().await;
        assert!(kcp.send_window_size() > 32);
        assert!(kcp.receive_window_size() > 128);
    }

    #[tokio::test]
    async fn rebind_switches_output_socket() {
        let first_socket = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let second_socket = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let new_socket = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let new_addr = new_socket.local_addr().unwrap();
        let second_addr = second_socket.local_addr().unwrap();

        let codec = Arc::new(PacketCodec::new(EncryptionMode::AesGcm, "pw"));
        let first = KcpChannel::new(
            codec.clone(),
            config(0x3344_5566),
            first_socket,
            second_addr,
        )
        .await
        .unwrap();
        let second = KcpChannel::new(codec, config(0x3344_5566), second_socket, new_addr)
            .await
            .unwrap();

        let old = first.rebind(new_socket, second_addr).unwrap();
        assert_eq!(first.local_addr().unwrap(), new_addr);
        assert_ne!(old.local_addr().unwrap(), new_addr);

        first.send(b"after rebind").await.unwrap();
        first.update(1000).await.unwrap();

        let mut datagram = vec![0u8; 2048];
        let (size, from) = timeout(
            Duration::from_secs(1),
            second.socket().recv_from(&mut datagram),
        )
        .await
        .expect("重绑后应从新 socket 发出")
        .unwrap();
        assert_eq!(from, new_addr);
        second.process_datagram(&datagram[..size]).await.unwrap();

        let mut buffer = vec![0u8; 32];
        assert_eq!(second.receive(&mut buffer).await.unwrap(), 12);
        assert_eq!(&buffer[..12], b"after rebind");
    }

    #[tokio::test]
    async fn tampered_datagram_is_rejected() {
        let first_socket = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let second_socket = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let first_addr = first_socket.local_addr().unwrap();
        let second_addr = second_socket.local_addr().unwrap();

        let codec = Arc::new(PacketCodec::new(EncryptionMode::AesGcm, "pw"));
        let first = KcpChannel::new(codec.clone(), config(7), first_socket, second_addr)
            .await
            .unwrap();
        let second = KcpChannel::new(codec, config(7), second_socket, first_addr)
            .await
            .unwrap();

        first.send(b"hello").await.unwrap();
        first.update(1000).await.unwrap();

        let mut datagram = vec![0u8; 2048];
        let (size, _) = timeout(
            Duration::from_secs(1),
            second.socket().recv_from(&mut datagram),
        )
        .await
        .unwrap()
        .unwrap();
        datagram[0] ^= 0xFF;
        assert!(second.process_datagram(&datagram[..size]).await.is_err());
    }
}
