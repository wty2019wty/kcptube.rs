// SPDX-License-Identifier: GPL-3.0-only

//! TCP 会话封装（对应 C++ `tcp_session` / `tcp_server` 的句柄部分）。
//!
//! C++ 版在 `tcp_session` 里同时管理读缓冲、写队列与超时回调；
//! Rust 版先用 tokio 的异步读写表达背压，超时与断开处理交给上层模式。

use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{
    TcpListener, TcpStream,
    tcp::{OwnedReadHalf, OwnedWriteHalf},
};

/// 一条已建立的 TCP 连接。
pub struct TcpSession {
    stream: TcpStream,
}

impl TcpSession {
    pub async fn connect(addr: SocketAddr) -> std::io::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        Ok(Self { stream })
    }

    pub fn from_stream(stream: TcpStream) -> std::io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self { stream })
    }

    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        self.stream.peer_addr()
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.stream.local_addr()
    }

    pub async fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.stream.read(buffer).await
    }

    pub async fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.stream.write(data).await
    }

    pub async fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.stream.write_all(data).await
    }

    pub async fn shutdown(&mut self) -> std::io::Result<()> {
        self.stream.shutdown().await
    }

    /// 拆成读写两半，便于在 `select!` 中并发处理
    pub fn into_split(self) -> (OwnedReadHalf, OwnedWriteHalf) {
        self.stream.into_split()
    }

    pub fn into_inner(self) -> TcpStream {
        self.stream
    }
}

/// TCP 监听器。
pub struct TcpListenerSession {
    listener: TcpListener,
}

impl TcpListenerSession {
    pub async fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(addr).await?,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub async fn accept(&self) -> std::io::Result<(TcpSession, SocketAddr)> {
        let (stream, peer) = self.listener.accept().await?;
        Ok((TcpSession::from_stream(stream)?, peer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn listener_accepts_and_exchanges_data() {
        let listener = TcpListenerSession::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let client = tokio::spawn(async move {
            let mut session = TcpSession::connect(addr).await.unwrap();
            session.write_all(b"hello tcp").await.unwrap();
            session.shutdown().await.unwrap();
        });

        let (mut server_session, peer) = listener.accept().await.unwrap();
        assert_eq!(peer.ip(), addr.ip());
        let mut buffer = vec![0u8; 32];
        let size = server_session.read(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..size], b"hello tcp");

        client.await.unwrap();
    }

    #[tokio::test]
    async fn split_read_write_halves() {
        let listener = TcpListenerSession::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let client = tokio::spawn(async move {
            let mut session = TcpSession::connect(addr).await.unwrap();
            session.write_all(b"ping").await.unwrap();
            session.shutdown().await.unwrap();
        });

        let (session, _) = listener.accept().await.unwrap();
        let (mut reader, _writer) = session.into_split();
        let mut buffer = [0u8; 4];
        reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"ping");

        client.await.unwrap();
    }
}
