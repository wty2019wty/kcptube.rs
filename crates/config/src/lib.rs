// SPDX-License-Identifier: GPL-3.0-only

//! 配置文件解析。
//!
//! 目标：与 C++ 版（`legacy-cpp/src/shares/configurations.cpp`，约 1000 行）兼容，
//! 同一份 `.conf` 在两端必须解析出等价参数。
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
//! ```
//!
//! `kcp` 的取值预设：`regular1`~`regular5`、`fast1`~`fast6`、`manual`。

use kcptube_crypto::EncryptionMode;
use std::collections::HashMap;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunningMode {
    Unknown,
    Server,
    Client,
    Relay,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpVersionOnly {
    NotSet,
    Ipv4,
    Ipv6,
}

/// 一条配置（对应 C++ `user_settings`）。
#[derive(Debug, Clone)]
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

    pub stun_server: String,
    pub update_ipv4_path: String,
    pub update_ipv6_path: String,
    pub log_directory: PathBuf,
    pub config_filename: PathBuf,

    /// 自定义 IP 映射：(输入地址, 输入端口) -> (映射地址, 映射端口)
    pub user_input_mappings: HashMap<(String, u16), (String, u16)>,
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
            stun_server: String::new(),
            update_ipv4_path: String::new(),
            update_ipv6_path: String::new(),
            log_directory: PathBuf::new(),
            config_filename: PathBuf::new(),
            user_input_mappings: HashMap::new(),
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
    #[error("第 {line} 行：{message}")]
    Parse { line: usize, message: String },
    #[error("配置解析尚未实现（里程碑 1）")]
    NotImplemented,
}

impl UserSettings {
    /// 从若干行文本解析配置（每行一条 `key=value`）。
    pub fn parse_lines(_lines: &[String]) -> Result<Self, Vec<ConfigError>> {
        Err(vec![ConfigError::NotImplemented])
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
        settings.config_filename = path;
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_cpp_defaults() {
        let settings = UserSettings::default();
        assert_eq!(settings.mtu, -1);
        assert_eq!(settings.dynamic_port_refresh, -1);
        assert!(settings.blast);
        assert_eq!(settings.encryption, EncryptionMode::Empty);
    }
}
