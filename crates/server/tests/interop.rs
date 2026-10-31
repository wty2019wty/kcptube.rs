// SPDX-License-Identifier: GPL-3.0-only

//! M3 端到端测试：Rust 客户端 ↔ Rust 服务端 ↔ 本地 TCP echo。
//!
//! 覆盖：`initialise` 握手、KCP 数据通道、`raw_data` 双向转发、
//! 保活收发、连接关闭后的会话回收。

use kcptube_client::Client;
use kcptube_config::UserSettings;
use kcptube_server::Server;
use std::net::TcpListener as StdTcpListener;
use std::net::UdpSocket as StdUdpSocket;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

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

async fn echo_server() -> (u16, Vec<JoinHandle<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let tcp_handle = tokio::spawn(async move {
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

    // 同一端口上的 UDP echo（协议不同，可以复用端口号）
    let udp = tokio::net::UdpSocket::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let udp_handle = tokio::spawn(async move {
        let mut buffer = vec![0u8; 65536];
        loop {
            let Ok((size, peer)) = udp.recv_from(&mut buffer).await else {
                break;
            };
            let _ = udp.send_to(&buffer[..size], peer).await;
        }
    });

    (port, vec![tcp_handle, udp_handle])
}

/// 启动一套 echo + 服务端 + 客户端，返回客户端本地端口与后台任务。
async fn start_pair(keep_alive: u16) -> (u16, Vec<JoinHandle<()>>) {
    start_pair_with(keep_alive, None).await
}

async fn start_pair_with(keep_alive: u16, fec: Option<(u8, u8)>) -> (u16, Vec<JoinHandle<()>>) {
    let (echo_port, echo_tasks) = echo_server().await;
    let server_port = free_udp_port();
    let client_port = free_tcp_port();

    let fec_line = fec
        .map(|(original, redundant)| format!("fec={original}:{redundant}\n"))
        .unwrap_or_default();

    let server_settings = settings(&format!(
        "mode=server\n\
         listen_on=127.0.0.1\n\
         listen_port={server_port}\n\
         destination_address=127.0.0.1\n\
         destination_port={echo_port}\n\
         encryption_password=test-password\n\
         encryption_algorithm=AES-GCM\n\
         kcp=regular3\n\
         {fec_line}\
         keep_alive={keep_alive}\n"
    ));
    let client_settings = settings(&format!(
        "mode=client\n\
         listen_on=127.0.0.1\n\
         listen_port={client_port}\n\
         destination_address=127.0.0.1\n\
         destination_port={server_port}\n\
         encryption_password=test-password\n\
         encryption_algorithm=AES-GCM\n\
         kcp=regular3\n\
         {fec_line}\
         keep_alive={keep_alive}\n"
    ));

    let server_task = tokio::spawn(async move {
        let _ = Server::new(server_settings).run().await;
    });
    let client_task = tokio::spawn(async move {
        let _ = Client::new(client_settings).run().await;
    });
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut tasks = vec![server_task, client_task];
    tasks.extend(echo_tasks);
    (client_port, tasks)
}

fn stop(tasks: Vec<JoinHandle<()>>) {
    for task in tasks {
        task.abort();
    }
}

/// 在一条已拆分的连接上来回发送若干段
async fn exchange<R, W>(reader: &mut R, writer: &mut W, prefix: &str, count: u8)
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    for index in 0..count {
        let message = format!("{prefix} #{index}");
        writer.write_all(message.as_bytes()).await.unwrap();
        let mut buffer = vec![0u8; message.len()];
        reader.read_exact(&mut buffer).await.unwrap();
        assert_eq!(buffer, message.as_bytes());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_server_tcp_roundtrip() {
    let (client_port, tasks) = start_pair(0).await;
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
            .await
            .unwrap();
        let (mut reader, mut writer) = stream.split();
        exchange(&mut reader, &mut writer, "hello kcptube", 3).await;
    })
    .await;

    stop(tasks);
    result.expect("10 秒内应完成握手与 TCP 往返");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_roundtrip() {
    let (client_port, tasks) = start_pair(0).await;

    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect(("127.0.0.1", client_port)).await.unwrap();
        let mut buffer = vec![0u8; 2048];
        for index in 0..3u8 {
            let message = format!("udp hello #{index}");
            // 首个数据报会触发握手（客户端会先缓存再发送），失败则重发
            let mut received = None;
            for _ in 0..20 {
                socket.send(message.as_bytes()).await.unwrap();
                match tokio::time::timeout(Duration::from_millis(300), socket.recv(&mut buffer))
                    .await
                {
                    Ok(Ok(size)) => {
                        received = Some(buffer[..size].to_vec());
                        break;
                    }
                    _ => continue,
                }
            }
            let received = received.expect("应在 6 秒内收到 UDP 回显");
            assert_eq!(received, message.as_bytes());
        }
    })
    .await;

    stop(tasks);
    result.expect("UDP 往返应在 15 秒内完成");
}

/// 大块数据往返（覆盖 KCP 分片/重组与 FEC 冗余）
async fn bulk_roundtrip(port: u16, size: usize) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let (mut reader, mut writer) = stream.split();
    let payload: Vec<u8> = (0..size).map(|index| (index * 7 % 256) as u8).collect();
    writer.write_all(&payload).await.unwrap();
    let mut received = vec![0u8; payload.len()];
    reader.read_exact(&mut received).await.unwrap();
    assert_eq!(received, payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fec_tcp_roundtrip() {
    // `fec=4:2`：每 4 个原始包附带 2 个冗余包
    let (client_port, tasks) = start_pair_with(0, Some((4, 2))).await;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
            .await
            .unwrap();
        let (mut reader, mut writer) = stream.split();
        exchange(&mut reader, &mut writer, "fec hello", 5).await;
        // 32 KiB 会触发 KCP 分片，从而产生多组 FEC 冗余
        bulk_roundtrip(client_port, 32 * 1024).await;
    })
    .await;

    stop(tasks);
    result.expect("FEC 模式下应在 20 秒内完成往返");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_alive_and_reconnect() {
    let (client_port, tasks) = start_pair(1).await;

    let result = tokio::time::timeout(Duration::from_secs(20), async {
        // 同一条连接跨过两个保活周期后仍能收发
        {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
                .await
                .unwrap();
            let (mut reader, mut writer) = stream.split();
            exchange(&mut reader, &mut writer, "keepalive-1", 2).await;
            tokio::time::sleep(Duration::from_secs(3)).await;
            exchange(&mut reader, &mut writer, "keepalive-2", 2).await;
        }

        // 断开后重新建立两条连接，验证会话回收与再次握手
        for index in 0..2u8 {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
                .await
                .unwrap();
            let (mut reader, mut writer) = stream.split();
            exchange(&mut reader, &mut writer, &format!("reconnect-{index}"), 1).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    })
    .await;

    stop(tasks);
    result.expect("保活与重连应在 20 秒内完成");
}
