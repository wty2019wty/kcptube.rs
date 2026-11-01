// SPDX-License-Identifier: GPL-3.0-only

//! 已知问题 #13 的回归测试：多个配置文件必须**并发**启动。
//!
//! 用两个 `mode=server` 配置启动真实二进制，确认两个 UDP 监听端口都被绑定。
//! 修复前 `main` 逐个 `await`，第一个常驻配置会独占进程，第二个端口永远不会监听。

use std::io::{BufRead, BufReader};
use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 让系统分配一个临时端口，确认 TCP / UDP 都可用后释放给子进程使用。
fn free_port() -> u16 {
    let tcp = TcpListener::bind(("127.0.0.1", 0)).expect("分配临时端口");
    let port = tcp.local_addr().expect("读取临时端口").port();
    drop(tcp);
    assert!(
        UdpSocket::bind(("127.0.0.1", port)).is_ok(),
        "临时端口 {port} 的 UDP 已被占用"
    );
    port
}

fn write_config(dir: &Path, name: &str, port: u16) -> PathBuf {
    let path = dir.join(name);
    let text = format!(
        "mode=server\n\
         listen_on=127.0.0.1\n\
         listen_port={port}\n\
         destination_address=127.0.0.1\n\
         destination_port=9\n\
         encryption_password=test-password\n\
         encryption_algorithm=AES-GCM\n\
         kcp=regular3\n"
    );
    std::fs::write(&path, text).expect("写入测试配置");
    path
}

#[test]
fn multiple_profiles_start_concurrently() {
    let dir = std::env::temp_dir().join(format!("kcptube-multi-profile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("创建临时目录");

    let port_a = free_port();
    let mut port_b = free_port();
    while port_b == port_a {
        port_b = free_port();
    }
    let config_a = write_config(&dir, "a.conf", port_a);
    let config_b = write_config(&dir, "b.conf", port_b);

    let mut child = Command::new(env!("CARGO_BIN_EXE_kcptube"))
        .arg(&config_a)
        .arg(&config_b)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("启动 kcptube");

    let stdout = child.stdout.take().expect("接管子进程 stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // 日志行形如 `INFO 监听 UDP [::]:12345`（见 server 的启动日志）。
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut seen_a, mut seen_b) = (false, false);
    while !(seen_a && seen_b) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let Ok(line) = rx.recv_timeout(left) else {
            break;
        };
        if line.contains("UDP") {
            seen_a |= line.contains(&port_a.to_string());
            seen_b |= line.contains(&port_b.to_string());
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(seen_a, "第一个配置的监听端口 {port_a} 未启动");
    assert!(
        seen_b,
        "第二个配置的监听端口 {port_b} 未启动（多配置文件没有并发启动）"
    );
}
