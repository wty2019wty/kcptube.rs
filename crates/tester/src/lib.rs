// SPDX-License-Identifier: GPL-3.0-only

//! 连通性测试（`--try`，对应 C++ `modes/tester.cpp`）。
//!
//! 对配置里的「每个目标地址 × 每个目标端口」并发建立 conv 0 握手、
//! 发送 `test_connection`，30 秒内收到响应即视为成功。结果按地址分组，
//! 输出格式与 C++ `PrintResults` 逐字一致：
//!
//! ```text
//! Testing...
//! Connection Test Result #0 of "example.com":
//! Selected IP Address: 1.2.3.4
//! Success: ALL (5)
//! Failure: NONE
//!
//! ```
//!
//! 只支持 `mode=client`（取根配置）与 `mode=relay`（取 `[forwarder]` 段）；
//! 其余模式与 C++ 一致，不产生任何输出。

use kcptube_config::{RunningMode, UserSettings};
use kcptube_protocol::constant::{HANDSHAKE_TIMEOUT, PACKET_LENGTH};
use kcptube_protocol::packet::{Feature, create_test_connection_packet, unpack_inner};
use kcptube_transport::{
    MAX_INNER_MESSAGE, PacketCodec, TransportError, now_ms, open_handshake_channel,
    resolve_endpoints_filtered,
};
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::time::{Instant, interval};

#[derive(Debug, Error)]
pub enum TesterError {
    #[error("配置不完整：{0}")]
    Startup(String),
    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
    #[error("传输层错误：{0}")]
    Transport(#[from] TransportError),
}

/// 单个地址的测试结果。
#[derive(Debug, Clone)]
pub struct TestResult {
    /// 在 `destination_address` 列表中的下标
    pub index: usize,
    /// 配置里写的目标地址（域名或 IP）
    pub address: String,
    /// 实际解析并使用的 IP
    pub selected_ip: String,
    pub success_ports: BTreeSet<u16>,
    pub failure_ports: BTreeSet<u16>,
    /// 参与测试的端口总数（用于判断 `ALL` / `NONE`）
    pub total_ports: usize,
}

/// 连通性测试入口。
pub struct Tester {
    settings: UserSettings,
    timeout: Duration,
}

impl Tester {
    pub fn new(settings: UserSettings) -> Self {
        Self {
            settings,
            timeout: Duration::from_secs(HANDSHAKE_TIMEOUT as u64),
        }
    }

    /// 覆盖单次探测的超时（默认 30 秒）。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 执行测试并打印结果。
    ///
    /// 返回每个地址的结果；不支持的模式返回空列表（与 C++ 一致）。
    pub async fn run(&self) -> Result<Vec<TestResult>, TesterError> {
        let Some(source) = self.source_settings()? else {
            return Ok(Vec::new());
        };
        if source.destination_ports.is_empty() || source.destination_address_list.is_empty() {
            return Err(TesterError::Startup(
                "destination_address / destination_port not set".to_string(),
            ));
        }

        println!("Testing...");

        let codec = Arc::new(PacketCodec::new(
            source.encryption,
            &source.encryption_password,
        ));
        let fec = source.fec_parameters();
        let kcp_mtu = source.kcp_mtu;
        let want_ipv4 = source.ip_version_only.want_ipv4();
        let outbound = source.outbound_bandwidth;
        let inbound = source.inbound_bandwidth;

        let mut results = Vec::new();
        for (index, address) in source.destination_address_list.iter().enumerate() {
            let selected = match resolve_endpoints_filtered(address, 0, want_ipv4)
                .await
                .ok()
                .and_then(|candidates| candidates.into_iter().next())
            {
                Some(endpoint) => endpoint,
                None => {
                    // 与 C++ 的差异：C++ 解析失败会终止整个测试，这里记为全部失败继续
                    tracing::warn!("无法解析目标地址 {address}");
                    results.push(TestResult {
                        index,
                        address: address.clone(),
                        selected_ip: String::new(),
                        success_ports: BTreeSet::new(),
                        failure_ports: source.destination_ports.iter().copied().collect(),
                        total_ports: source.destination_ports.len(),
                    });
                    continue;
                }
            };

            let mut tasks = Vec::new();
            for port in &source.destination_ports {
                let target = SocketAddr::new(selected.ip(), *port);
                let codec = codec.clone();
                let timeout = self.timeout;
                tasks.push((
                    *port,
                    tokio::spawn(async move {
                        probe(codec, fec, kcp_mtu, outbound, inbound, target, timeout).await
                    }),
                ));
            }

            let mut success_ports = BTreeSet::new();
            let mut failure_ports = BTreeSet::new();
            for (port, task) in tasks {
                match task.await {
                    Ok(true) => {
                        success_ports.insert(port);
                    }
                    _ => {
                        failure_ports.insert(port);
                    }
                }
            }

            results.push(TestResult {
                index,
                address: address.clone(),
                selected_ip: selected.ip().to_string(),
                success_ports,
                failure_ports,
                total_ports: source.destination_ports.len(),
            });
        }

        print!("{}", format_results(&results));
        Ok(results)
    }

    /// 按 C++ `test_mode::start` 取测试所用的配置段。
    fn source_settings(&self) -> Result<Option<&UserSettings>, TesterError> {
        match self.settings.mode {
            RunningMode::Client => Ok(Some(&self.settings)),
            RunningMode::Relay => self
                .settings
                .egress
                .as_deref()
                .map(Some)
                .ok_or_else(|| TesterError::Startup("配置文件缺少 [forwarder]".to_string())),
            _ => Ok(None),
        }
    }
}

/// 对单个 `地址:端口` 探测：建 conv 0 通道、发 `test_connection`、等待响应。
async fn probe(
    codec: Arc<PacketCodec>,
    fec: Option<(u8, u8)>,
    kcp_mtu: i32,
    outbound: u64,
    inbound: u64,
    target: SocketAddr,
    timeout: Duration,
) -> bool {
    let (channel, socket) = match open_handshake_channel(codec, kcp_mtu, target, fec).await {
        Ok(pair) => pair,
        Err(error) => {
            tracing::debug!("无法建立到 {target} 的握手通道：{error}");
            return false;
        }
    };
    channel
        .apply_handshake_tuning(true, outbound, inbound)
        .await;

    if channel
        .send(&create_test_connection_packet())
        .await
        .is_err()
    {
        return false;
    }
    if channel.update(now_ms()).await.is_err() {
        return false;
    }

    let deadline = Instant::now() + timeout;
    let mut ticker = interval(Duration::from_millis(5));
    let mut datagram = vec![0u8; PACKET_LENGTH + 256];
    let mut inner = vec![0u8; MAX_INNER_MESSAGE];

    loop {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::select! {
            _ = ticker.tick() => {
                if channel.update(now_ms()).await.is_err() {
                    return false;
                }
            }
            result = socket.recv_from(&mut datagram) => {
                let Ok((size, _)) = result else { continue };
                if channel.process_datagram(&datagram[..size]).await.is_err() {
                    continue;
                }
            }
            _ = tokio::time::sleep_until(deadline) => return false,
        }

        loop {
            match channel.receive(&mut inner).await {
                Ok(size) => {
                    let Ok((feature, _, _)) = unpack_inner(&inner[..size]) else {
                        continue;
                    };
                    // `test_connection` 与 `keep_alive` 共用同一 feature 值
                    if feature == Feature::KeepAlive {
                        return true;
                    }
                }
                Err(TransportError::Kcp(kcptube_kcp::KcpError::WouldBlock)) => break,
                Err(error) => {
                    tracing::debug!("探测 {target} 失败：{error}");
                    return false;
                }
            }
        }
    }
}

/// 拼出与 C++ `PrintResults` 一致的文本（末尾每个结果后一个空行）。
pub fn format_results(results: &[TestResult]) -> String {
    let mut output = String::new();
    for result in results {
        output.push_str(&format!(
            "Connection Test Result #{} of \"{}\":\n",
            result.index, result.address
        ));
        output.push_str(&format!("Selected IP Address: {}\n", result.selected_ip));
        output.push_str(&format_ports(
            "Success",
            &result.success_ports,
            result.total_ports,
        ));
        output.push_str(&format_ports(
            "Failure",
            &result.failure_ports,
            result.total_ports,
        ));
        output.push('\n');
    }
    output
}

fn format_ports(label: &str, ports: &BTreeSet<u16>, total: usize) -> String {
    if ports.is_empty() {
        return format!("{label}: NONE\n");
    }
    if ports.len() == total {
        return format!("{label}: ALL ({})\n", ports.len());
    }
    let mut output = format!("{label} ({}): ", ports.len());
    for port in ports {
        output.push_str(&format!("{port} "));
    }
    output.push('\n');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(
        index: usize,
        address: &str,
        success: &[u16],
        failure: &[u16],
        total: usize,
    ) -> TestResult {
        TestResult {
            index,
            address: address.to_string(),
            selected_ip: "1.2.3.4".to_string(),
            success_ports: success.iter().copied().collect(),
            failure_ports: failure.iter().copied().collect(),
            total_ports: total,
        }
    }

    #[test]
    fn all_and_none_match_cpp() {
        let output = format_results(&[
            result(0, "example.com", &[3000, 3001, 3002], &[], 3),
            result(1, "empty.example", &[], &[3000, 3001, 3002], 3),
        ]);
        let expected = "\
Connection Test Result #0 of \"example.com\":
Selected IP Address: 1.2.3.4
Success: ALL (3)
Failure: NONE

Connection Test Result #1 of \"empty.example\":
Selected IP Address: 1.2.3.4
Success: NONE
Failure: ALL (3)

";
        assert_eq!(output, expected);
    }

    #[test]
    fn partial_lists_ports_in_order() {
        let output = format_results(&[result(0, "host", &[3002, 3000], &[3001], 3)]);
        assert!(
            output.contains("Success (2): 3000 3002 \n"),
            "部分成功应按端口升序：{output}"
        );
        assert!(output.contains("Failure (1): 3001 \n"), "{output}");
    }

    #[test]
    fn unsupported_mode_has_no_source() {
        let settings = UserSettings {
            mode: RunningMode::Server,
            ..Default::default()
        };
        assert!(
            Tester::new(settings)
                .source_settings()
                .expect("server 模式应可判定")
                .is_none()
        );

        let settings = UserSettings {
            mode: RunningMode::Relay,
            ..Default::default()
        };
        assert!(
            Tester::new(settings)
                .source_settings()
                .expect_err("缺少 [forwarder] 应报错")
                .to_string()
                .contains("[forwarder]")
        );
    }
}
