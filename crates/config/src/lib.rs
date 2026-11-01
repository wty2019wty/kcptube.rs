// SPDX-License-Identifier: GPL-3.0-only

//! 配置文件解析。
//!
//! 目标：与 C++ 版（`legacy-cpp/src/shares/configurations.cpp`，约 1000 行）兼容，
//! 同一份 `.conf` 在两端必须解析出等价参数。
//!
//! ## 解析流程（与 C++ `parse_from_args` 一致）
//!
//! 1. `parse_running_mode`：先只扫描 `mode=`，确定运行模式；
//! 2. `parse_the_rest`：按当前分段（根 / `[listener]` / `[forwarder]`）解析其余参数；
//! 3. `check_settings`：校验参数、展开 KCP 预设、计算 MTU、解析日志路径。
//!
//! 任一步骤产生错误都会立刻返回，错误文本尽量与 C++ 版逐字一致，
//! 以便 `--check-config` 的输出可以直接对比。
//!
//! ## 已确认的参数名（来自 C++ `strhash` 分支）
//!
//! ```text
//! mode  listen_on  listen_port  destination_address  destination_port
//! destination_dnstxt  destination  listen
//! encryption_algorithm  encryption_password
//! kcp  kcp_mtu  kcp_nodelay  kcp_interval  kcp_resend  kcp_nc
//! kcp_sndwnd  kcp_rcvwnd  mtu
//! fec  dport_refresh  udp_timeout  keep_alive  mux_tunnels
//! outbound_bandwidth  inbound_bandwidth
//! ipv4_only  ipv6_only  blast
//! fib_ingress  fib_egress  stun_server
//! update_ipv4  update_ipv6  log_path
//! [listener] [forwarder] [custom_input] [custom_input_tcp] [custom_input_udp]
//! ```
//!
//! `kcp` 的取值预设：`regular1`~`regular5`、`fast1`~`fast6`、`manual`。

mod parser;

use kcptube_crypto::EncryptionMode;
use kcptube_kcp::KcpConfig;
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunningMode {
    Unknown,
    Server,
    Client,
    Relay,
    /// relay 的入口段（`[listener]`）
    RelayIngress,
    /// relay 的出口段（`[forwarder]`）
    RelayEgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KcpMode {
    Unknown,
    Regular1,
    Regular2,
    Regular3,
    Regular4,
    Regular5,
    Fast1,
    Fast2,
    Fast3,
    Fast4,
    Fast5,
    Fast6,
    Manual,
}

/// IP 版本限制。C++ 用位掩码表示，两个选项同时为真时值为 3。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpVersionOnly {
    NotSet,
    Ipv4,
    Ipv6,
    /// `ipv4_only` 与 `ipv6_only` 同时出现（非法配置，校验阶段会报错）
    Both,
}

impl IpVersionOnly {
    pub const fn is_set(self) -> bool {
        !matches!(self, IpVersionOnly::NotSet)
    }

    /// 按配置选择通配监听地址。
    ///
    /// 与 C++ 一致：只有 `ipv4_only` 才用 `0.0.0.0`；未指定或 `ipv6_only`
    /// 时都用 `[::]`。C++ 的 `udp_server::initialise` 只在 `ipv6_only` 时
    /// 打开 `IPV6_V6ONLY`，所以未指定时是**双栈**（同时收 IPv4 与 IPv6），
    /// 这也是 C++ 版默认能同时被两种地址族访问的原因。
    pub const fn wildcard_ip(self) -> std::net::IpAddr {
        match self {
            IpVersionOnly::Ipv4 => std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            _ => std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        }
    }

    /// 监听 IPv6 地址时是否打开 `IPV6_V6ONLY`。
    ///
    /// 未指定或 `ipv4_only` 时为 `false`（双栈），只有 `ipv6_only` 才是 `true`。
    pub const fn ipv6_v6only(self) -> bool {
        matches!(self, IpVersionOnly::Ipv6)
    }

    /// 解析目标地址时应限定的地址族。
    pub const fn want_ipv4(self) -> Option<bool> {
        match self {
            IpVersionOnly::Ipv4 => Some(true),
            IpVersionOnly::Ipv6 => Some(false),
            _ => None,
        }
    }
}

/// 一条配置（对应 C++ `user_settings`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSettings {
    pub mode: RunningMode,
    pub kcp_setting: KcpMode,
    pub encryption: EncryptionMode,
    pub encryption_password: String,

    pub listen_on: Vec<String>,
    pub listen_ports: Vec<u16>,
    pub destination_address_list: Vec<String>,
    pub destination_ports: Vec<u16>,
    pub destination_dnstxt: String,

    pub mtu: i32,
    pub kcp_mtu: i32,
    pub kcp_nodelay: i32,
    pub kcp_interval: i32,
    pub kcp_resend: i32,
    pub kcp_nc: i32,
    pub kcp_sndwnd: u32,
    pub kcp_rcvwnd: u32,

    pub fec_original_packet_count: u8,
    pub fec_redundant_packet_count: u8,
    pub dynamic_port_refresh: i16,
    pub udp_timeout: u16,
    pub keep_alive: u16,
    pub mux_tunnels: u16,

    pub outbound_bandwidth: u64,
    pub inbound_bandwidth: u64,
    pub ip_version_only: IpVersionOnly,
    pub blast: bool,

    pub ignore_listen_address: bool,
    pub ignore_listen_port: bool,
    pub ignore_destination_address: bool,
    pub ignore_destination_port: bool,

    pub fib_ingress: i32,
    pub fib_egress: i32,

    pub stun_server: String,
    pub update_ipv4_path: String,
    pub update_ipv6_path: String,

    pub log_directory: PathBuf,
    pub log_ip_address: PathBuf,
    pub log_messages: PathBuf,
    pub log_status: PathBuf,

    pub config_filename: PathBuf,

    /// 未限定协议的输入地址映射
    pub user_input_mappings: BTreeMap<(String, u16), (String, u16)>,
    /// 仅 TCP 的输入地址映射（`[custom_input_tcp]`）
    pub user_input_mappings_tcp: BTreeMap<(String, u16), (String, u16)>,
    /// 仅 UDP 的输入地址映射（`[custom_input_udp]`）
    pub user_input_mappings_udp: BTreeMap<(String, u16), (String, u16)>,
    /// 是否出现过 `[custom_input]` 段（即使段内为空，C++ 也不会报错）
    pub has_user_input_mappings: bool,
    /// 是否出现过 `[custom_input_tcp]` 段
    pub has_user_input_mappings_tcp: bool,
    /// 是否出现过 `[custom_input_udp]` 段
    pub has_user_input_mappings_udp: bool,

    /// relay 入口段（`[listener]`）
    pub ingress: Option<Box<UserSettings>>,
    /// relay 出口段（`[forwarder]`）
    pub egress: Option<Box<UserSettings>>,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            mode: RunningMode::Unknown,
            kcp_setting: KcpMode::Unknown,
            encryption: EncryptionMode::Empty,
            encryption_password: String::new(),
            listen_on: Vec::new(),
            listen_ports: Vec::new(),
            destination_address_list: Vec::new(),
            destination_ports: Vec::new(),
            destination_dnstxt: String::new(),
            mtu: -1,
            kcp_mtu: -1,
            kcp_nodelay: -1,
            kcp_interval: -1,
            kcp_resend: -1,
            kcp_nc: -1,
            kcp_sndwnd: 0,
            kcp_rcvwnd: 0,
            fec_original_packet_count: 0,
            fec_redundant_packet_count: 0,
            dynamic_port_refresh: -1,
            udp_timeout: 0,
            keep_alive: 0,
            mux_tunnels: 0,
            outbound_bandwidth: 0,
            inbound_bandwidth: 0,
            ip_version_only: IpVersionOnly::NotSet,
            blast: true,
            ignore_listen_address: false,
            ignore_listen_port: false,
            ignore_destination_address: false,
            ignore_destination_port: false,
            fib_ingress: -1,
            fib_egress: -1,
            stun_server: String::new(),
            update_ipv4_path: String::new(),
            update_ipv6_path: String::new(),
            log_directory: PathBuf::new(),
            log_ip_address: PathBuf::new(),
            log_messages: PathBuf::new(),
            log_status: PathBuf::new(),
            config_filename: PathBuf::new(),
            user_input_mappings: BTreeMap::new(),
            user_input_mappings_tcp: BTreeMap::new(),
            user_input_mappings_udp: BTreeMap::new(),
            has_user_input_mappings: false,
            has_user_input_mappings_tcp: false,
            has_user_input_mappings_udp: false,
            ingress: None,
            egress: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("无法读取配置文件 {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// 与 C++ 版一致的错误文本
    #[error("{0}")]
    Message(String),
}

impl UserSettings {
    /// 从若干行文本解析配置（每行一条 `key=value`）。
    pub fn parse_lines(lines: &[String]) -> Result<Self, Vec<ConfigError>> {
        let mut settings = Self::default();
        let mut errors = parser::parse_running_mode(lines, &mut settings);
        if !errors.is_empty() {
            return Err(to_errors(errors));
        }

        errors = parser::parse_the_rest(lines, &mut settings);
        if !errors.is_empty() {
            return Err(to_errors(errors));
        }

        parser::check_settings(&mut settings, &mut errors);
        if errors.is_empty() {
            Ok(settings)
        } else {
            Err(to_errors(errors))
        }
    }

    /// 从文件解析配置。
    pub fn parse_file(path: impl Into<PathBuf>) -> Result<Self, Vec<ConfigError>> {
        let path = path.into();
        let content = std::fs::read_to_string(&path).map_err(|e| {
            vec![ConfigError::Io {
                path: path.clone(),
                source: e,
            }]
        })?;

        let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
        let mut settings = Self::parse_lines(&lines)?;
        settings.config_filename = path.clone();

        // 对应 C++ main.cpp：log_status = log_directory / (配置文件文件名 + "_status.log")
        if !settings.log_directory.as_os_str().is_empty() {
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            settings.log_status = settings
                .log_directory
                .join(format!("{file_name}_status.log"));
        }

        Ok(settings)
    }

    /// 该配置是否启用了 FEC
    pub const fn fec_enabled(&self) -> bool {
        self.fec_original_packet_count > 0 && self.fec_redundant_packet_count > 0
    }

    /// FEC 参数 `(原始包数 k, 冗余包数 r)`；未启用时为 `None`。
    pub fn fec_parameters(&self) -> Option<(u8, u8)> {
        self.fec_enabled().then_some((
            self.fec_original_packet_count,
            self.fec_redundant_packet_count,
        ))
    }

    /// 数据通道的 KCP 参数（对应 C++ `kcp_mappings` 构造时的 `current_settings` 映射）。
    ///
    /// 握手通道使用固定参数，见 `kcptube_kcp::KcpConfig::handshake`。
    pub fn kcp_config(&self, conv: u32) -> KcpConfig {
        KcpConfig {
            conv,
            mtu: self.kcp_mtu.max(50) as usize,
            nodelay: self.kcp_nodelay,
            interval: self.kcp_interval.max(1) as u32,
            resend: self.kcp_resend.max(0) as u32,
            no_congestion_control: self.kcp_nc != 0,
            send_window: self.kcp_sndwnd,
            receive_window: self.kcp_rcvwnd,
            stream_mode: false,
        }
    }

    /// 日志文件是否可用（对应 C++ `log_status` 是否为空）
    pub fn has_status_log(&self) -> bool {
        !self.log_status.as_os_str().is_empty()
    }
}

fn to_errors(messages: Vec<String>) -> Vec<ConfigError> {
    messages.into_iter().map(ConfigError::Message).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(text: &str) -> Result<UserSettings, Vec<ConfigError>> {
        let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
        UserSettings::parse_lines(&lines)
    }

    fn sample(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("service")
            .join(name);
        std::fs::read_to_string(path).expect("样例配置应存在")
    }

    #[test]
    fn defaults_match_cpp_defaults() {
        let settings = UserSettings::default();
        assert_eq!(settings.mtu, -1);
        assert_eq!(settings.dynamic_port_refresh, -1);
        assert!(settings.blast);
        assert_eq!(settings.encryption, EncryptionMode::Empty);
        assert_eq!(settings.fib_ingress, -1);
    }

    #[test]
    fn parse_client_sample() {
        let settings = parse(&sample("config_sample_client.conf")).expect("应解析成功");
        assert_eq!(settings.mode, RunningMode::Client);
        assert_eq!(settings.kcp_setting, KcpMode::Regular3);
        assert_eq!(settings.listen_ports, vec![5099]);
        assert_eq!(settings.destination_ports, vec![30000]);
        assert_eq!(settings.destination_address_list, vec!["127.0.0.1"]);
        assert_eq!(settings.encryption, EncryptionMode::AesGcm);
        assert_eq!(settings.encryption_password, "qwerty1234");

        // regular3 预设
        assert_eq!(settings.kcp_nodelay, 0);
        assert_eq!(settings.kcp_interval, 1);
        assert_eq!(settings.kcp_resend, 2);
        assert_eq!(settings.kcp_nc, 1);
        assert_eq!(settings.kcp_sndwnd, 1024);
        assert_eq!(settings.kcp_rcvwnd, 1024);

        // 未指定 mtu 时使用默认值
        assert_eq!(settings.kcp_mtu, 1418);
        // 未指定 dport_refresh 时使用默认值
        assert_eq!(settings.dynamic_port_refresh, 60);
        assert_eq!(settings.udp_timeout, 180);
    }

    #[test]
    fn parse_server_range_sample() {
        let settings = parse(&sample("config_sample_server_range.conf")).expect("应解析成功");
        assert_eq!(settings.mode, RunningMode::Server);
        assert_eq!(settings.listen_ports.len(), 10001);
        assert_eq!(settings.listen_ports.first(), Some(&30000));
        assert_eq!(settings.listen_ports.last(), Some(&40000));
        assert_eq!(settings.kcp_sndwnd, 4096);
        assert_eq!(settings.kcp_rcvwnd, 4096);
    }

    #[test]
    fn port_lists_and_ranges() {
        assert_eq!(
            parser::string_to_port_numbers("3000-3002", &mut Vec::new(), "listen"),
            vec![3000, 3001, 3002]
        );
        assert_eq!(
            parser::string_to_port_numbers("3005, 3001,3001", &mut Vec::new(), "listen"),
            vec![3001, 3005]
        );
        assert_eq!(
            parser::string_to_address_list("b.com, a.com ,a.com"),
            vec!["a.com", "b.com"]
        );
    }

    #[test]
    fn invalid_port_reports_cpp_message() {
        let mut errors = Vec::new();
        let ports = parser::string_to_port_numbers("0", &mut errors, "listen");
        assert!(ports.is_empty());
        assert_eq!(errors, vec!["invalid listen_port number: 0"]);
    }

    #[test]
    fn missing_mode_is_rejected() {
        let errors = parse("listen_port=5099").expect_err("缺少 mode 应报错");
        assert!(
            errors
                .iter()
                .any(|e| e.to_string() == "running mode is not set")
        );
    }

    #[test]
    fn unknown_option_is_rejected() {
        let errors = parse("mode=client\nfoo=bar").expect_err("未知参数应报错");
        assert!(
            errors
                .iter()
                .any(|e| e.to_string() == "unknow option: foo=bar")
        );
    }

    #[test]
    fn kcp_presets_match_cpp() {
        let cases = [
            ("fast1", (1, 0, 2, 1, 2048)),
            ("fast2", (2, 0, 2, 1, 2048)),
            ("fast3", (1, 1, 3, 1, 2048)),
            ("fast4", (2, 1, 3, 1, 2048)),
            ("fast5", (1, 1, 4, 1, 2048)),
            ("fast6", (2, 1, 4, 1, 2048)),
            ("regular1", (1, 1, 5, 1, 1024)),
            ("regular2", (2, 1, 5, 1, 1024)),
            ("regular4", (0, 15, 2, 1, 1024)),
            ("regular5", (0, 30, 2, 1, 1024)),
        ];

        for (name, (nodelay, interval, resend, nc, wnd)) in cases {
            let text = format!(
                "mode=client\nlisten_port=5099\ndestination_port=80\ndestination_address=127.0.0.1\nkcp={name}"
            );
            let settings = parse(&text).expect("应解析成功");
            assert_eq!(settings.kcp_nodelay, nodelay, "{name}");
            assert_eq!(settings.kcp_interval, interval, "{name}");
            assert_eq!(settings.kcp_resend, resend, "{name}");
            assert_eq!(settings.kcp_nc, nc, "{name}");
            assert_eq!(settings.kcp_sndwnd, wnd, "{name}");
        }
    }

    #[test]
    fn fec_and_mtu_calculation() {
        let settings =
            parse("mode=server\nlisten_port=30000\ndestination_port=80\ndestination_address=127.0.0.1\nfec=10:3\nmtu=1400")
                .expect("应解析成功");
        assert_eq!(settings.fec_original_packet_count, 10);
        assert_eq!(settings.fec_redundant_packet_count, 3);
        assert!(settings.fec_enabled());
        // 1400 - iv_checksum_block(2) - (ip 36 + udp 4 + data 1 + fec 13 + container 2)
        assert_eq!(settings.kcp_mtu, 1400 - 2 - 56);
    }

    #[test]
    fn fec_bad_format_is_rejected() {
        let errors = parse("mode=server\nlisten_port=30000\nfec=10").expect_err("应报错");
        assert!(
            errors
                .iter()
                .any(|e| e.to_string() == "invalid fec format: 10")
        );
    }

    #[test]
    fn kcp_config_maps_settings() {
        let settings = parse(&sample("config_sample_client.conf")).expect("应解析成功");
        let config = settings.kcp_config(0x1234_5678);
        assert_eq!(config.conv, 0x1234_5678);
        assert_eq!(config.mtu, settings.kcp_mtu as usize);
        assert_eq!(config.nodelay, settings.kcp_nodelay);
        assert_eq!(config.interval, settings.kcp_interval as u32);
        assert_eq!(config.resend, settings.kcp_resend as u32);
        assert_eq!(config.no_congestion_control, settings.kcp_nc != 0);
        assert_eq!(config.send_window, settings.kcp_sndwnd);
        assert_eq!(config.receive_window, settings.kcp_rcvwnd);
        assert!(!config.stream_mode);
    }

    #[test]
    fn kcp_config_clamps_fast_preset_interval() {
        // C++ 预设 fast1/fast2 的 kcp_interval 为 0，KCP 侧统一按 1ms 驱动
        let settings = parse(
            "mode=client\nlisten_port=5099\ndestination_port=80\ndestination_address=127.0.0.1\nkcp=fast1",
        )
        .expect("应解析成功");
        assert_eq!(settings.kcp_interval, 0);
        assert_eq!(settings.kcp_config(0).interval, 1);
    }

    #[test]
    fn fec_parameters_matches_fec_enabled() {
        let without = parse(
            "mode=server\nlisten_port=30000\ndestination_port=80\ndestination_address=127.0.0.1",
        )
        .expect("应解析成功");
        assert!(!without.fec_enabled());
        assert_eq!(without.fec_parameters(), None);

        let with = parse(
            "mode=server\nlisten_port=30000\ndestination_port=80\ndestination_address=127.0.0.1\nfec=10:3",
        )
        .expect("应解析成功");
        assert!(with.fec_enabled());
        assert_eq!(with.fec_parameters(), Some((10, 3)));
    }

    #[test]
    fn handshake_config_matches_cpp() {
        let config = KcpConfig::handshake(1418);
        assert_eq!(config.conv, 0);
        assert_eq!(config.mtu, 1418);
        assert_eq!(config.nodelay, 1);
        assert_eq!(config.interval, 1);
        assert_eq!(config.resend, 3);
        assert!(config.no_congestion_control);
        assert_eq!(config.send_window, 32);
        assert_eq!(config.receive_window, 128);
        // MTU 下限与 C++ 一致
        assert_eq!(KcpConfig::handshake(10).mtu, 50);
    }

    #[test]
    fn bandwidth_units_match_cpp() {
        let mut errors = Vec::new();
        assert_eq!(parser::bandwidth_from_string("8K", &mut errors), 1024);
        assert_eq!(parser::bandwidth_from_string("8k", &mut errors), 1000);
        assert_eq!(
            parser::bandwidth_from_string("8M", &mut errors),
            1024 * 1024
        );
        assert_eq!(parser::bandwidth_from_string("64", &mut errors), 0);
        assert!(errors.is_empty());
    }

    #[test]
    fn custom_input_mapping() {
        let text = "\
mode=client
listen_port={}
destination_port=80
destination_address=127.0.0.1
[custom_input]
127.0.0.1:5099 -> 10.0.0.1:80
[custom_input_tcp]
127.0.0.1:5100 -> example.com:443
";
        let settings = parse(text).expect("应解析成功");
        assert!(settings.listen_ports.is_empty());
        assert_eq!(
            settings
                .user_input_mappings
                .get(&("127.0.0.1".to_string(), 5099)),
            Some(&("10.0.0.1".to_string(), 80))
        );
        assert_eq!(
            settings
                .user_input_mappings_tcp
                .get(&("127.0.0.1".to_string(), 5100)),
            Some(&("example.com".to_string(), 443))
        );
    }

    #[test]
    fn relay_sections() {
        let text = "\
mode=relay
[listener]
listen_port=30000
[forwarder]
destination_address=127.0.0.1
destination_port=80
";
        let settings = parse(text).expect("应解析成功");
        assert_eq!(settings.mode, RunningMode::Relay);
        let ingress = settings.ingress.as_ref().expect("listener 段");
        let egress = settings.egress.as_ref().expect("forwarder 段");
        assert_eq!(ingress.mode, RunningMode::RelayIngress);
        assert_eq!(ingress.listen_ports, vec![30000]);
        assert_eq!(egress.mode, RunningMode::RelayEgress);
        assert_eq!(egress.destination_ports, vec![80]);
        // relay 的 udp_timeout 会被追加 5 秒
        assert_eq!(ingress.udp_timeout, 185);
        assert_eq!(egress.udp_timeout, 185);
    }

    #[test]
    fn relay_requires_both_sections() {
        let errors = parse("mode=relay\n[listener]\nlisten_port=30000").expect_err("应报错");
        assert!(
            errors
                .iter()
                .any(|e| e.to_string() == "[forwarder] is missing")
        );
    }

    #[test]
    fn both_ip_versions_are_rejected() {
        let errors = parse(
            "mode=server\nlisten_port=30000\ndestination_port=80\ndestination_address=127.0.0.1\nipv6_only=yes\nipv4_only=yes",
        )
        .expect_err("应报错");
        assert!(
            errors
                .iter()
                .any(|e| e.to_string() == "Both ipv4_only and ipv6_only are set as true")
        );
    }

    #[test]
    fn ip_version_selects_wildcard_and_family() {
        assert_eq!(
            IpVersionOnly::Ipv4.wildcard_ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
        );
        assert_eq!(
            IpVersionOnly::Ipv6.wildcard_ip(),
            std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
        );
        // 未指定时与 C++ 一致：IPv6 通配 + 双栈
        assert_eq!(
            IpVersionOnly::NotSet.wildcard_ip(),
            std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
        );
        assert!(!IpVersionOnly::NotSet.ipv6_v6only());
        assert!(!IpVersionOnly::Ipv4.ipv6_v6only());
        assert!(IpVersionOnly::Ipv6.ipv6_v6only());
        assert_eq!(IpVersionOnly::Ipv4.want_ipv4(), Some(true));
        assert_eq!(IpVersionOnly::Ipv6.want_ipv4(), Some(false));
        assert_eq!(IpVersionOnly::NotSet.want_ipv4(), None);
    }
}
