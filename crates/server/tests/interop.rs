// SPDX-License-Identifier: GPL-3.0-only

//! M3 端到端测试：Rust 客户端 ↔ Rust 服务端 ↔ 本地 TCP echo。
//!
//! 覆盖：`initialise` 握手、KCP 数据通道、`raw_data` 双向转发、
//! 保活收发、连接关闭后的会话回收。

use kcptube_client::Client;
use kcptube_config::UserSettings;
use kcptube_relay::Relay;
use kcptube_server::Server;
use std::collections::HashSet;
use std::net::UdpSocket as StdUdpSocket;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// 测试端口段。选在 10000–19999，避开 Linux / Windows 常见的临时端口段，
/// 以免和测试内部 `bind(:0)` 得到的临时端口相撞。
const PORT_RANGE_START: u16 = 10000;
const PORT_RANGE_END: u16 = 20000;

/// 进程内统一分配端口：保证不重复，并同时校验 TCP 与 UDP 都可用。
///
/// 客户端会在同一个端口上同时监听 TCP 和 UDP，因此仅检查其中一种协议
/// 不足以保证不冲突。
fn allocated_ports() -> &'static Mutex<HashSet<u16>> {
    static PORTS: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
    PORTS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn port_is_free(port: u16) -> bool {
    match (
        StdTcpListener::bind(("127.0.0.1", port)),
        StdUdpSocket::bind(("127.0.0.1", port)),
    ) {
        (Ok(tcp), Ok(udp)) => {
            drop(tcp);
            drop(udp);
            true
        }
        _ => false,
    }
}

/// 分配 `count` 个连续端口，返回起始端口。
fn allocate_ports(count: u16) -> u16 {
    assert!(count > 0, "至少分配一个端口");
    let mut used = allocated_ports().lock().expect("端口分配器锁被污染");
    let mut base = PORT_RANGE_START;
    while u32::from(base) + u32::from(count) <= u32::from(PORT_RANGE_END) {
        let end = base + count - 1;
        if (base..=end).all(|port| !used.contains(&port) && port_is_free(port)) {
            used.extend(base..=end);
            return base;
        }
        base += 1;
    }
    panic!("在 {PORT_RANGE_START}-{PORT_RANGE_END} 内找不到 {count} 个连续空闲端口");
}

fn settings(text: &str) -> UserSettings {
    let lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
    UserSettings::parse_lines(&lines).expect("测试配置应合法")
}

async fn echo_server() -> (u16, Vec<JoinHandle<()>>) {
    let port = allocate_ports(1);
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
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
    start_pair_with_options(PairOptions {
        keep_alive,
        ..PairOptions::default()
    })
    .await
}

async fn start_pair_with(keep_alive: u16, fec: Option<(u8, u8)>) -> (u16, Vec<JoinHandle<()>>) {
    start_pair_with_options(PairOptions {
        keep_alive,
        fec,
        ..PairOptions::default()
    })
    .await
}

#[derive(Default)]
struct PairOptions {
    keep_alive: u16,
    fec: Option<(u8, u8)>,
    mux_tunnels: u16,
    /// 非 0 时在客户端与服务端之间插入一个每 `drop_every` 个报文丢 1 个的 UDP 代理
    drop_every: u32,
}

/// 进程内 UDP 丢包代理：每 `drop_every` 个报文丢 1 个（双向共用一个计数器），
/// 用于验证 FEC 在人为丢包下的恢复能力。
async fn lossy_proxy(server_port: u16, drop_every: u32) -> (u16, Vec<JoinHandle<()>>) {
    let client_side = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server_side = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = client_side.local_addr().unwrap().port();

    let client_side = std::sync::Arc::new(client_side);
    let server_side = std::sync::Arc::new(server_side);
    let last_client = std::sync::Arc::new(Mutex::new(None::<SocketAddr>));
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

    let upstream = {
        let client_side = client_side.clone();
        let server_side = server_side.clone();
        let last_client = last_client.clone();
        let counter = counter.clone();
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 65536];
            loop {
                let Ok((size, peer)) = client_side.recv_from(&mut buffer).await else {
                    break;
                };
                *last_client.lock().expect("代理锁被污染") = Some(peer);
                let count = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count % drop_every == 0 {
                    continue;
                }
                let _ = server_side
                    .send_to(&buffer[..size], ("127.0.0.1", server_port))
                    .await;
            }
        })
    };

    let downstream = {
        let client_side = client_side.clone();
        let server_side = server_side.clone();
        let last_client = last_client.clone();
        let counter = counter.clone();
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 65536];
            loop {
                let Ok((size, _)) = server_side.recv_from(&mut buffer).await else {
                    break;
                };
                let count = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count % drop_every == 0 {
                    continue;
                }
                let peer = *last_client.lock().expect("代理锁被污染");
                if let Some(peer) = peer {
                    let _ = client_side.send_to(&buffer[..size], peer).await;
                }
            }
        })
    };

    (proxy_port, vec![upstream, downstream])
}

async fn start_pair_with_options(options: PairOptions) -> (u16, Vec<JoinHandle<()>>) {
    let PairOptions {
        keep_alive,
        fec,
        mux_tunnels,
        drop_every,
    } = options;
    let (echo_port, echo_tasks) = echo_server().await;
    let server_port = allocate_ports(1);
    let client_port = allocate_ports(1);

    // 需要丢包时，客户端连到代理端口，由代理转发到真实服务端
    let mut proxy_tasks = Vec::new();
    let client_target_port = if drop_every > 0 {
        let (proxy_port, tasks) = lossy_proxy(server_port, drop_every).await;
        proxy_tasks = tasks;
        proxy_port
    } else {
        server_port
    };

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
    let mux_line = if mux_tunnels > 0 {
        format!("mux_tunnels={mux_tunnels}\n")
    } else {
        String::new()
    };
    let client_settings = settings(&format!(
        "mode=client\n\
         listen_on=127.0.0.1\n\
         listen_port={client_port}\n\
         destination_address=127.0.0.1\n\
         destination_port={client_target_port}\n\
         encryption_password=test-password\n\
         encryption_algorithm=AES-GCM\n\
         kcp=regular3\n\
         {fec_line}\
         {mux_line}\
         keep_alive={keep_alive}\n"
    ));

    let server_task = tokio::spawn(async move {
        let _ = Server::new(server_settings).run().await;
    });
    // 先等服务端完成 UDP 绑定，再启动客户端：mux 模式下客户端启动时就会
    // 立即握手，若服务端尚未监听，Windows 会对 ICMP 端口不可达返回
    // ConnectionRefused，导致客户端直接退出（C++ 版由定时器重试）。
    tokio::time::sleep(Duration::from_millis(300)).await;
    let client_task = tokio::spawn(async move {
        let _ = Client::new(client_settings).run().await;
    });
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut tasks = vec![server_task, client_task];
    tasks.extend(proxy_tasks);
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
async fn fec_recovers_under_packet_loss() {
    // 每 10 个 UDP 报文丢 1 个（双向），验证 FEC 冗余能减少重传并完整送达。
    let (client_port, tasks) = start_pair_with_options(PairOptions {
        fec: Some((4, 2)),
        drop_every: 10,
        ..PairOptions::default()
    })
    .await;

    let result = tokio::time::timeout(Duration::from_secs(40), async {
        bulk_roundtrip(client_port, 32 * 1024).await;
    })
    .await;

    stop(tasks);
    result.expect("FEC 开启时 10% 丢包下 32 KiB 数据应完整到达");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mux_tcp_and_udp_roundtrip() {
    // 两条共享隧道，本地 TCP/UDP 连接按 connection_id 复用
    let (client_port, tasks) = start_pair_with_options(PairOptions {
        mux_tunnels: 2,
        ..PairOptions::default()
    })
    .await;

    let result = tokio::time::timeout(Duration::from_secs(20), async {
        // 两条并发 TCP 连接应各自独立，互不串包
        let first = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
                .await
                .unwrap();
            let (mut reader, mut writer) = stream.split();
            exchange(&mut reader, &mut writer, "mux A", 4).await;
        });
        let second = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
                .await
                .unwrap();
            let (mut reader, mut writer) = stream.split();
            exchange(&mut reader, &mut writer, "mux B", 4).await;
            bulk_roundtrip(client_port, 32 * 1024).await;
        });
        first.await.unwrap();
        second.await.unwrap();

        // UDP 源地址同样复用 mux 通道
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect(("127.0.0.1", client_port)).await.unwrap();
        let mut buffer = vec![0u8; 2048];
        for index in 0..3u8 {
            let message = format!("mux udp #{index}");
            socket.send(message.as_bytes()).await.unwrap();
            let size = tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut buffer))
                .await
                .expect("应在 5 秒内收到 mux UDP 回显")
                .unwrap();
            assert_eq!(&buffer[..size], message.as_bytes());
        }
    })
    .await;

    stop(tasks);
    result.expect("mux 模式应在 20 秒内完成 TCP/UDP 往返");
}

/// `ignore_listen_*` + `[custom_input*]`：客户端只在映射端口监听，
/// 目标地址随握手包交给服务端；服务端忽略自己的 destination 配置。
///
/// `ignore_address` 选择服务端用 `destination_address={}` 还是
/// `destination_port={}` 来关闭默认目标，两者在 C++ 里走同一个判断。
async fn start_custom_input_pair(
    mux_tunnels: u16,
    ignore_address: bool,
) -> (u16, Vec<JoinHandle<()>>) {
    let (echo_port, echo_tasks) = echo_server().await;
    let server_port = allocate_ports(1);
    let client_port = allocate_ports(1);

    let server_destination = if ignore_address {
        "destination_address={}\n".to_string()
    } else {
        "destination_port={}\n".to_string()
    };
    let server_settings = settings(&format!(
        "mode=server\n\
         listen_on=127.0.0.1\n\
         listen_port={server_port}\n\
         {server_destination}\
         encryption_password=test-password\n\
         encryption_algorithm=AES-GCM\n\
         kcp=regular3\n\
         keep_alive=0\n"
    ));
    let mux_line = if mux_tunnels > 0 {
        format!("mux_tunnels={mux_tunnels}\n")
    } else {
        String::new()
    };
    let client_settings = settings(&format!(
        "mode=client\n\
         listen_port={{}}\n\
         destination_address=127.0.0.1\n\
         destination_port={server_port}\n\
         encryption_password=test-password\n\
         encryption_algorithm=AES-GCM\n\
         kcp=regular3\n\
         {mux_line}\
         keep_alive=0\n\
         [custom_input]\n\
         127.0.0.1:{client_port}->127.0.0.1:{echo_port}\n"
    ));

    let server_task = tokio::spawn(async move {
        let _ = Server::new(server_settings).run().await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let client_task = tokio::spawn(async move {
        let _ = Client::new(client_settings).run().await;
    });
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut tasks = vec![server_task, client_task];
    tasks.extend(echo_tasks);
    (client_port, tasks)
}

/// 通过 `[custom_input]` 映射端口做一次 UDP 往返。
async fn custom_udp_exchange(client_port: u16) {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.connect(("127.0.0.1", client_port)).await.unwrap();
    let mut buffer = vec![0u8; 2048];
    for index in 0..3u8 {
        let message = format!("custom udp #{index}");
        let mut received = None;
        for _ in 0..20 {
            socket.send(message.as_bytes()).await.unwrap();
            match tokio::time::timeout(Duration::from_millis(300), socket.recv(&mut buffer)).await {
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_input_tcp_roundtrip() {
    let (client_port, tasks) = start_custom_input_pair(0, false).await;
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
            .await
            .unwrap();
        let (mut reader, mut writer) = stream.split();
        exchange(&mut reader, &mut writer, "custom tcp", 3).await;
    })
    .await;

    stop(tasks);
    result.expect("custom_input 映射的 TCP 往返应在 15 秒内完成");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_input_udp_roundtrip() {
    let (client_port, tasks) = start_custom_input_pair(0, false).await;
    let result =
        tokio::time::timeout(Duration::from_secs(15), custom_udp_exchange(client_port)).await;

    stop(tasks);
    result.expect("custom_input 映射的 UDP 往返应在 15 秒内完成");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_input_ignore_destination_address() {
    // 服务端用 `destination_address={}`（而不是 `destination_port={}`）关闭
    // 默认目标：C++ 对这两个开关走同一个判断，目标完全由客户端映射决定。
    let (client_port, tasks) = start_custom_input_pair(0, true).await;
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
            .await
            .unwrap();
        let (mut reader, mut writer) = stream.split();
        exchange(&mut reader, &mut writer, "ignore address", 3).await;
    })
    .await;

    stop(tasks);
    result.expect("ignore_destination_address 下 TCP 往返应在 15 秒内完成");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn custom_input_mux_roundtrip() {
    // mux 模式下目标地址通过 `pre_connect_custom_address` 下发，
    // 与 C++ `mux_tell_server_connect_address` 的行为对齐。
    let (client_port, tasks) = start_custom_input_pair(1, false).await;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
            .await
            .unwrap();
        let (mut reader, mut writer) = stream.split();
        exchange(&mut reader, &mut writer, "custom mux tcp", 3).await;
        custom_udp_exchange(client_port).await;
    })
    .await;

    stop(tasks);
    result.expect("custom_input + mux 的 TCP/UDP 往返应在 20 秒内完成");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mux_hundred_concurrent_tcp() {
    // M4 验收：mux 下 100 并发 TCP 连接稳定。
    let (client_port, tasks) = start_pair_with_options(PairOptions {
        mux_tunnels: 4,
        ..PairOptions::default()
    })
    .await;

    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let mut handles = Vec::new();
        for index in 0..100u32 {
            handles.push(tokio::spawn(async move {
                let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
                    .await
                    .unwrap();
                let (mut reader, mut writer) = stream.split();
                exchange(&mut reader, &mut writer, &format!("conn {index}"), 2).await;
            }));
        }
        for handle in handles {
            handle.await.expect("并发连接任务不应 panic");
        }
    })
    .await;

    stop(tasks);
    result.expect("100 并发 mux TCP 连接应在 60 秒内全部完成");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn port_hopping_keeps_connection() {
    let (echo_port, echo_tasks) = echo_server().await;
    let base = allocate_ports(3);
    let end = base + 2;
    let client_port = allocate_ports(1);
    let log_dir = std::env::temp_dir().join(format!("kcptube-hop-{client_port}"));
    std::fs::create_dir_all(&log_dir).unwrap();

    let mut server_settings = settings(&format!(
        "mode=server\n\
         listen_on=127.0.0.1\n\
         listen_port={base}-{end}\n\
         destination_address=127.0.0.1\n\
         destination_port={echo_port}\n\
         encryption_password=test-password\n\
         encryption_algorithm=AES-GCM\n\
         kcp=regular3\n\
         keep_alive=1\n"
    ));
    let mut client_settings = settings(&format!(
        "mode=client\n\
         listen_on=127.0.0.1\n\
         listen_port={client_port}\n\
         destination_address=127.0.0.1\n\
         destination_port={base}-{end}\n\
         encryption_password=test-password\n\
         encryption_algorithm=AES-GCM\n\
         kcp=regular3\n\
         dport_refresh=1\n\
         log_path={}\n\
         keep_alive=1\n",
        log_dir.display()
    ));
    // 绕过解析器的 20 秒下限，测试中每 1 秒跳跃一次
    server_settings.dynamic_port_refresh = 1;
    client_settings.dynamic_port_refresh = 1;

    let server_task = tokio::spawn(async move {
        let _ = Server::new(server_settings).run().await;
    });
    // 同上：等服务端绑定端口范围后再启动客户端
    tokio::time::sleep(Duration::from_millis(300)).await;
    let client_task = tokio::spawn(async move {
        let _ = Client::new(client_settings).run().await;
    });
    tokio::time::sleep(Duration::from_millis(600)).await;

    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
            .await
            .unwrap();
        let (mut reader, mut writer) = stream.split();
        exchange(&mut reader, &mut writer, "hop-before", 2).await;

        // 至少经历两次端口跳跃后再收发
        tokio::time::sleep(Duration::from_secs(3)).await;
        exchange(&mut reader, &mut writer, "hop-after", 2).await;
    })
    .await;

    let mut tasks = vec![server_task, client_task];
    tasks.extend(echo_tasks);
    stop(tasks);
    result.expect("端口跳跃期间连接应保持可用");

    // 确认确实发生过切换，而不是连接一直停留在原端口
    let log = std::fs::read_to_string(log_dir.join("client_output.log")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&log_dir);
    assert!(log.contains("端口跳跃"), "客户端日志应记录端口跳跃：{log}");
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

/// Mux 客户端先于服务端启动时：客户端不应退出，本地连接应排队等待隧道就绪。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mux_client_queues_until_server_starts() {
    let (echo_port, echo_tasks) = echo_server().await;
    let server_port = allocate_ports(1);
    let client_port = allocate_ports(1);

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
         kcp=regular3\n\
         mux_tunnels=1\n"
    ));

    // 先启动客户端：此时服务端还不存在，客户端应保持监听并在后台重试建链
    let client_task = tokio::spawn(async move {
        let _ = Client::new(client_settings).run().await;
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 隧道尚未建立时建立本地连接并写入数据：连接应被排队保留，而不是直接丢弃
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
        .await
        .unwrap();
    stream.write_all(b"late-start").await.unwrap();

    // 再启动服务端，客户端应自动重试建链并处理已排队的连接
    let server_task = tokio::spawn(async move {
        let _ = Server::new(server_settings).run().await;
    });

    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut buffer = vec![0u8; "late-start".len()];
        stream.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"late-start");
    })
    .await;

    let mut tasks = vec![client_task, server_task];
    tasks.extend(echo_tasks);
    stop(tasks);
    result.expect("服务端后启动时，客户端应保留本地连接并在建链后完成转发");
}

/// 启动「echo + 服务端 + relay + 客户端」四段，客户端连到 relay。
///
/// 拓扑：`client --(A)--> relay --(B)--> server --> echo`，
/// 两段的加密/KCP 参数故意不同，用来验证 relay 会按段各自解码与重编码。
async fn start_relay_pair(keep_alive: u16) -> (u16, Vec<JoinHandle<()>>) {
    let (echo_port, echo_tasks) = echo_server().await;
    let server_port = allocate_ports(1);
    let relay_port = allocate_ports(1);
    let client_port = allocate_ports(1);

    let server_settings = settings(&format!(
        "mode=server\n\
         listen_on=127.0.0.1\n\
         listen_port={server_port}\n\
         destination_address=127.0.0.1\n\
         destination_port={echo_port}\n\
         encryption_password=server-password\n\
         encryption_algorithm=AES-OCB\n\
         kcp=regular3\n\
         keep_alive={keep_alive}\n"
    ));
    // [listener] 面向客户端：AES-GCM + regular3
    // [forwarder] 面向服务端：AES-OCB + fast3
    let relay_settings = settings(&format!(
        "mode=relay\n\
         [listener]\n\
         listen_on=127.0.0.1\n\
         listen_port={relay_port}\n\
         encryption_password=client-password\n\
         encryption_algorithm=AES-GCM\n\
         kcp=regular3\n\
         keep_alive={keep_alive}\n\
         [forwarder]\n\
         destination_address=127.0.0.1\n\
         destination_port={server_port}\n\
         encryption_password=server-password\n\
         encryption_algorithm=AES-OCB\n\
         kcp=fast3\n\
         keep_alive={keep_alive}\n"
    ));
    let client_settings = settings(&format!(
        "mode=client\n\
         listen_on=127.0.0.1\n\
         listen_port={client_port}\n\
         destination_address=127.0.0.1\n\
         destination_port={relay_port}\n\
         encryption_password=client-password\n\
         encryption_algorithm=AES-GCM\n\
         kcp=regular3\n\
         keep_alive={keep_alive}\n"
    ));

    let server_task = tokio::spawn(async move {
        let _ = Server::new(server_settings).run().await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let relay_task = tokio::spawn(async move {
        let _ = Relay::new(relay_settings).run().await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let client_task = tokio::spawn(async move {
        let _ = Client::new(client_settings).run().await;
    });
    tokio::time::sleep(Duration::from_millis(400)).await;

    let mut tasks = vec![server_task, relay_task, client_task];
    tasks.extend(echo_tasks);
    (client_port, tasks)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_tcp_roundtrip() {
    let (client_port, tasks) = start_relay_pair(0).await;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", client_port))
            .await
            .unwrap();
        let (mut reader, mut writer) = stream.split();
        exchange(&mut reader, &mut writer, "relay hello", 3).await;
        // 大块数据覆盖 KCP 分片与两段重编码
        let payload: Vec<u8> = (0..16_384).map(|index| (index * 7 % 256) as u8).collect();
        writer.write_all(&payload).await.unwrap();
        let mut received = vec![0u8; payload.len()];
        reader.read_exact(&mut received).await.unwrap();
        assert_eq!(received, payload);
    })
    .await;

    stop(tasks);
    result.expect("relay TCP 往返应在 20 秒内完成");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_udp_roundtrip() {
    let (client_port, tasks) = start_relay_pair(0).await;

    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect(("127.0.0.1", client_port)).await.unwrap();
        let mut buffer = vec![0u8; 2048];
        for index in 0..3u8 {
            let message = format!("relay udp #{index}");
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
    result.expect("relay UDP 往返应在 20 秒内完成");
}
