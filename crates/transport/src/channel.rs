// SPDX-License-Identifier: GPL-3.0-only

//! KCP 通道：把 `Kcp`、`PacketCodec` 与 `UdpEndpoint` 串成完整收发流水线。
//!
//! 发送：上层数据 → `Kcp::send` → KCP 段 → `PacketCodec::seal`
//!       （加时间戳 + 加密 + `iv_raw`）→ `UdpEndpoint::try_send_to`
//!
//! 接收：UDP 数据报 → `PacketCodec::open`（解密 + 拆包 + 时间窗）→ `Kcp::input`

use crate::fec::{FecReceiver, FecSender};
use crate::udp::UdpEndpoint;
use crate::{IncomingFrame, KcpEndpoint, PacketCodec, TransportError, now_secs, parse_frame};
use kcptube_kcp::{KcpConfig, KcpError};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// 一条完整的 KCP 数据通道。
pub struct KcpChannel {
    pub codec: Arc<PacketCodec>,
    pub endpoint: Arc<KcpEndpoint>,
    pub socket: UdpEndpoint,
    peer: Arc<std::sync::Mutex<SocketAddr>>,
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
        let endpoint = Arc::new(KcpEndpoint::new(config, peer));
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
            let socket = socket.clone();
            let peer_cell = peer_cell.clone();
            let output_bytes = output_bytes.clone();
            let fec_sender = fec_sender.clone();
            let mut kcp = endpoint.kcp.lock().await;
            kcp.set_output(Box::new(move |payload: &[u8]| {
                let target = peer_cell.lock().map(|guard| *guard).unwrap_or(peer);

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
            socket,
            peer: peer_cell,
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

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// 上层发送数据
    pub async fn send(&self, data: &[u8]) -> Result<(), TransportError> {
        let mut kcp = self.endpoint.kcp.lock().await;
        Ok(kcp.send(data)?)
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
        let (size, _peer) = self.socket.recv_from(buffer).await?;
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
            second.socket.recv_from(&mut datagram),
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
            first.socket.recv_from(&mut datagram),
        )
        .await
        .expect("应收到 ACK")
        .unwrap();
        first.process_datagram(&datagram[..size]).await.unwrap();
        assert_eq!(first.waiting_for_send().await, 0);
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
            second.socket.recv_from(&mut datagram),
        )
        .await
        .unwrap()
        .unwrap();
        datagram[0] ^= 0xFF;
        assert!(second.process_datagram(&datagram[..size]).await.is_err());
    }
}
