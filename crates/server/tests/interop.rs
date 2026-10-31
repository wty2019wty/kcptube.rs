// SPDX-License-Identifier: GPL-3.0-only

//! M3 端到端测试：Rust 客户端 ↔ Rust 服务端 ↔ 本地 TCP echo。
//!
//! 覆盖：`initialise` 握手、KCP 数据通道、`raw_data` 双向转发。

use kcptube_client::Client;
use kcptube_config::UserSettings;
use kcptube_server::Server;
use std::net::TcpListener as StdTcpListener;
use std::net::UdpSocket as StdUdpSocket;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn free_tcp_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn free_udp_port() -> u16 {
    let socket = StdUdpSocket::bind("127.0.0.1:0").unwrap();
    socket.local_addr().unwrap().port()
}

fn settings(text: &str) -> UserSettings {
    let lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
    UserSettings::parse_lines(&lines).expect("测试配置应合法")
}

async fn echo_server() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 4096];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(size) => {
                            if stream.write_all(&buffer[..size]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    (port, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_server_tcp_roundtrip() {
    let (echo_port, echo_task) = echo_server().await;
    let server_port = free_udp_port();
    let client_port = free_tcp_port();

    let server_settings = settings(&format!(
        "mode=server\n\
         listen_on=127.0.0.1\n\
         listen_port={server_port}\n\
         destination_address=127.0.0.1\n\
         destination_port={echo_port}\n\
         encryption_password=test-password\n\
         encryption_algorithm=AES-GCM\n\
         kcp=regular3\n"
    ));
    let client_settings = settings(&format!(
        "mode=client\n\
         listen_on=127.0.0.1\n\
         listen_port={client_port}\n\
         destination_address=127.0.0.1\n\
         destination_port={server_port}\n\
         encryption_password=test-password\n\
         encryption_algorithm=AES-GCM\n\
         kcp=regular3\n"
    ));

    let server_task = tokio::spawn(async move { Server::new(server_settings).run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let client_task = tokio::spawn(async move { Client::new(client_settings).run().await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let roundtrip = tokio::time::timeout(Duration::from_secs(10), async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
            .await
            .unwrap();
        for index in 0..3u8 {
            let message = format!("hello kcptube #{index}");
            stream.write_all(message.as_bytes()).await.unwrap();
            let mut buffer = vec![0u8; message.len()];
            stream.read_exact(&mut buffer).await.unwrap();
            assert_eq!(buffer, message.as_bytes());
        }
    })
    .await;

    server_task.abort();
    client_task.abort();
    echo_task.abort();
    roundtrip.expect("10 秒内应完成握手与 TCP 往返");
}
