// SPDX-License-Identifier: GPL-3.0-only

//! UDP 套接字封装（对应 C++ `udp_server` / `udp_client` 的句柄部分）。
//!
//! 使用 `socket2` 处理 `SO_REUSEADDR`、IPv6 双栈等选项，再交给 tokio 做异步收发。
//! `SO_SETFIB` 属于 FreeBSD 专有选项，等 M10 平台项再补。

use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;

/// 按对端地址族选择本地通配地址（`0.0.0.0:0` 或 `[::]:0`）。
///
/// 对应 C++ 中按 `endpoint.address().is_v4()` 选择 `udp::v4()` / `udp::v6()` 的写法。
/// 客户端握手、端口探测与服务端建立 UDP 目标套接字都使用它。
pub fn wildcard_addr(peer: SocketAddr) -> SocketAddr {
    if peer.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct UdpBindOptions {
    pub reuse_address: bool,
    /// `Some(true)` = `IPV6_V6ONLY`，`Some(false)` = 双栈，`None` = 使用系统默认
    pub only_v6: Option<bool>,
}

impl Default for UdpBindOptions {
    fn default() -> Self {
        Self {
            reuse_address: true,
            only_v6: None,
        }
    }
}

/// 可克隆的 UDP 端点（内部 `Arc` 共享异步与同步句柄）。
#[derive(Clone)]
pub struct UdpEndpoint {
    socket: Arc<UdpSocket>,
    sync_socket: Option<Arc<std::net::UdpSocket>>,
}

impl UdpEndpoint {
    pub async fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        Self::bind_with(addr, UdpBindOptions::default()).await
    }

    pub async fn bind_with(addr: SocketAddr, options: UdpBindOptions) -> std::io::Result<Self> {
        let domain = Domain::for_address(addr);
        let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        if options.reuse_address {
            socket.set_reuse_address(true)?;
        }
        if addr.is_ipv6() {
            if let Some(only_v6) = options.only_v6 {
                socket.set_only_v6(only_v6)?;
            }
        }
        socket.set_nonblocking(true)?;
        socket.bind(&addr.into())?;
        let std_socket: std::net::UdpSocket = socket.into();
        let sync_socket = std_socket.try_clone()?;
        Ok(Self {
            socket: Arc::new(UdpSocket::from_std(std_socket)?),
            sync_socket: Some(Arc::new(sync_socket)),
        })
    }

    /// 从已有的 tokio socket 构造（测试或上层自行 bind 时使用）
    pub fn from_socket(socket: UdpSocket) -> Self {
        Self {
            socket: Arc::new(socket),
            sync_socket: None,
        }
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub async fn recv_from(&self, buffer: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buffer).await
    }

    pub async fn send_to(&self, data: &[u8], peer: SocketAddr) -> std::io::Result<usize> {
        self.socket.send_to(data, peer).await
    }

    /// 同步非阻塞发送，供 KCP 的同步 output 回调使用。
    ///
    /// 优先走底层 `std::net::UdpSocket`，避免 tokio 的 readiness 状态
    /// 在同步回调里返回 `WouldBlock`。
    pub fn try_send_to(&self, data: &[u8], peer: SocketAddr) -> std::io::Result<usize> {
        if let Some(sync_socket) = &self.sync_socket {
            sync_socket.send_to(data, peer)
        } else {
            self.socket.try_send_to(data, peer)
        }
    }

    pub fn as_socket(&self) -> &UdpSocket {
        &self.socket
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bind_and_exchange_datagram() {
        let first = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let second = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();

        let peer = second.local_addr().unwrap();
        assert_eq!(first.try_send_to(b"ping", peer).unwrap(), 4);

        let mut buffer = [0u8; 16];
        let (size, from) = second.recv_from(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..size], b"ping");
        assert_eq!(from, first.local_addr().unwrap());
    }

    #[tokio::test]
    async fn reuse_address_allows_rebind() {
        let first = UdpEndpoint::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = first.local_addr().unwrap();
        drop(first);

        let second = UdpEndpoint::bind_with(
            addr,
            UdpBindOptions {
                reuse_address: true,
                only_v6: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(second.local_addr().unwrap(), addr);
    }
}
