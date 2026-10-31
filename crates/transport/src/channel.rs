// SPDX-License-Identifier: GPL-3.0-only

//! KCP 通道：把 `Kcp`、`PacketCodec` 与 `UdpEndpoint` 串成完整收发流水线。
//!
//! 发送：上层数据 → `Kcp::send` → KCP 段 → `PacketCodec::seal`
//!       （加时间戳 + 加密 + `iv_raw`）→ `UdpEndpoint::try_send_to`
//!
//! 接收：UDP 数据报 → `PacketCodec::open`（解密 + 拆包 + 时间窗）→ `Kcp::input`

use crate::udp::UdpEndpoint;
use crate::{KcpEndpoint, PacketCodec, TransportError};
use kcptube_kcp::{KcpConfig, KcpError};
use std::net::SocketAddr;
use std::sync::Arc;

/// 一条完整的 KCP 数据通道。
pub struct KcpChannel {
    pub codec: Arc<PacketCodec>,
    pub endpoint: Arc<KcpEndpoint>,
    pub socket: UdpEndpoint,
    peer: Arc<std::sync::Mutex<SocketAddr>>,
}

impl KcpChannel {
    /// 建立通道并注册 KCP 输出回调。
    pub async fn new(
        codec: Arc<PacketCodec>,
        config: KcpConfig,
        socket: UdpEndpoint,
        peer: SocketAddr,
    ) -> Result<Self, TransportError> {
        let endpoint = Arc::new(KcpEndpoint::new(config, peer));
        let peer_cell = Arc::new(std::sync::Mutex::new(peer));

        {
            let codec = codec.clone();
            let socket = socket.clone();
            let peer_cell = peer_cell.clone();
            let mut kcp = endpoint.kcp.lock().await;
            kcp.set_output(Box::new(move |payload: &[u8]| {
                let target = peer_cell.lock().map(|guard| *guard).unwrap_or(peer);
                let datagram = codec.seal(payload).map_err(|_| KcpError::OutputFailed)?;
                socket
                    .try_send_to(&datagram, target)
                    .map_err(|_| KcpError::OutputFailed)?;
                Ok(())
            }));
        }

        Ok(Self {
            codec,
            endpoint,
            socket,
            peer: peer_cell,
        })
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
        let (_, payload) = self.codec.open(datagram)?;
        self.input_payload(&payload).await
    }

    /// 处理一个已经解密、拆包后的 KCP 负载
    pub async fn input_payload(&self, payload: &[u8]) -> Result<(), TransportError> {
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
