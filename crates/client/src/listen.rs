// SPDX-License-Identifier: GPL-3.0-only

//! 本地监听项的计算（常规监听与 `[custom_input*]` 映射）。

use crate::ClientError;
use kcptube_config::{IpVersionOnly, UserSettings};
use kcptube_logging::{print_message_to_file, time_string_with_brackets};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

/// 一条本地监听项。
///
/// `remote` 是 `[custom_input*]` 映射右侧的固定目标地址；常规监听
/// （`listen_on` + `listen_port`）没有固定目标，由服务端配置决定。
pub(crate) struct ListenSpec {
    pub(crate) address: SocketAddr,
    pub(crate) tcp: bool,
    pub(crate) udp: bool,
    pub(crate) remote: Option<Arc<(String, u16)>>,
}

/// 计算需要监听的端点列表。
///
/// 对应 C++ `client_mode::multiple_listening_tcp` / `multiple_listening_udp`：
/// - 未设置 `ignore_listen_address` / `ignore_listen_port` 时，监听
///   `listen_on` 下每个地址的第一个 `listen_port`（TCP + UDP 同端口）；
/// - 设置后不再监听 `listen_on` / `listen_port`，改为逐条监听
///   `[custom_input]`（TCP + UDP）、`[custom_input_tcp]`（仅 TCP）、
///   `[custom_input_udp]`（仅 UDP）中的映射。
pub(crate) fn build_listen_specs(settings: &UserSettings) -> Result<Vec<ListenSpec>, ClientError> {
    let mut specs = Vec::new();

    if settings.ignore_listen_address || settings.ignore_listen_port {
        collect_mapping_specs(
            settings,
            &settings.user_input_mappings,
            true,
            true,
            &mut specs,
        )?;
        collect_mapping_specs(
            settings,
            &settings.user_input_mappings_tcp,
            true,
            false,
            &mut specs,
        )?;
        collect_mapping_specs(
            settings,
            &settings.user_input_mappings_udp,
            false,
            true,
            &mut specs,
        )?;
        return Ok(specs);
    }

    // 客户端只监听第一个端口（与 C++ `listen_ports.front()` 一致）
    let listen_port = *settings
        .listen_ports
        .first()
        .ok_or_else(|| ClientError::Startup("listen_port is not set".to_string()))?;
    let listen_addresses = if settings.listen_on.is_empty() {
        vec![settings.ip_version_only.wildcard_ip()]
    } else {
        settings
            .listen_on
            .iter()
            .map(|address| {
                address
                    .parse::<IpAddr>()
                    .map_err(|_| ClientError::Startup(format!("无效的 listen_on 地址：{address}")))
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    for ip in listen_addresses {
        if let Some(want_ipv4) = settings.ip_version_only.want_ipv4() {
            if ip.is_ipv4() != want_ipv4 {
                return Err(ClientError::Startup(format!(
                    "listen_on 地址 {ip} 与 ipv4_only/ipv6_only 配置冲突"
                )));
            }
        }
        specs.push(ListenSpec {
            address: SocketAddr::new(ip, listen_port),
            tcp: true,
            udp: true,
            remote: None,
        });
    }
    Ok(specs)
}

/// 把一张 `[custom_input*]` 映射表转换成监听项。
fn collect_mapping_specs(
    settings: &UserSettings,
    mappings: &std::collections::BTreeMap<(String, u16), (String, u16)>,
    tcp: bool,
    udp: bool,
    specs: &mut Vec<ListenSpec>,
) -> Result<(), ClientError> {
    for ((local_address, local_port), (remote_address, remote_port)) in mappings {
        let ip = if local_address.is_empty() {
            settings.ip_version_only.wildcard_ip()
        } else {
            local_address.parse::<IpAddr>().map_err(|_| {
                ClientError::Startup(format!("无效的 custom_input 本地地址：{local_address}"))
            })?
        };

        // 与 C++ 一致：只对 `ipv4_only` 过滤 IPv6 映射；
        // `ipv6_only` 下 C++ 不做过滤，绑定失败会在启动时暴露。
        if settings.ip_version_only == IpVersionOnly::Ipv4 && ip.is_ipv6() {
            let line = format!(
                "{}ipv4_only is set, ignoring IPv6 address{ip}\n",
                time_string_with_brackets()
            );
            eprintln!("{}", line.trim_end());
            let _ = print_message_to_file(&settings.log_messages, &line);
            continue;
        }

        specs.push(ListenSpec {
            address: SocketAddr::new(ip, *local_port),
            tcp,
            udp,
            remote: Some(Arc::new((remote_address.clone(), *remote_port))),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> UserSettings {
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        UserSettings::parse_lines(&lines).expect("测试配置应合法")
    }

    #[test]
    fn listen_specs_regular_listen() {
        let settings = parse(
            "mode=client\n\
             listen_on=127.0.0.1\n\
             listen_port=5000\n\
             destination_address=127.0.0.1\n\
             destination_port=6000\n\
             encryption_password=p\n\
             encryption_algorithm=aes-gcm\n",
        );
        let specs = build_listen_specs(&settings).expect("应能生成监听项");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].address, "127.0.0.1:5000".parse().unwrap());
        assert!(specs[0].tcp && specs[0].udp);
        assert!(specs[0].remote.is_none());
    }

    #[test]
    fn listen_specs_from_custom_input_sections() {
        let settings = parse(
            "mode=client\n\
             listen_port={}\n\
             destination_address=127.0.0.1\n\
             destination_port=6000\n\
             encryption_password=p\n\
             encryption_algorithm=aes-gcm\n\
             [custom_input_tcp]\n\
             127.0.0.1:7001->127.0.0.1:8080\n\
             [custom_input_udp]\n\
             127.0.0.1:7002->127.0.0.1:8081\n",
        );
        let specs = build_listen_specs(&settings).expect("应能生成监听项");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].address, "127.0.0.1:7001".parse().unwrap());
        assert!(specs[0].tcp && !specs[0].udp);
        assert_eq!(
            specs[0].remote.as_deref().map(|item| item.0.as_str()),
            Some("127.0.0.1")
        );
        assert_eq!(specs[0].remote.as_ref().map(|item| item.1), Some(8080));
        assert_eq!(specs[1].address, "127.0.0.1:7002".parse().unwrap());
        assert!(!specs[1].tcp && specs[1].udp);
        assert_eq!(specs[1].remote.as_ref().map(|item| item.1), Some(8081));
    }

    #[test]
    fn listen_specs_skip_ipv6_mapping_under_ipv4_only() {
        let settings = parse(
            "mode=client\n\
             listen_port={}\n\
             ipv4_only=yes\n\
             destination_address=127.0.0.1\n\
             destination_port=6000\n\
             encryption_password=p\n\
             encryption_algorithm=aes-gcm\n\
             [custom_input]\n\
             [::1]:7003->127.0.0.1:8080\n\
             127.0.0.1:7004->127.0.0.1:8080\n",
        );
        let specs = build_listen_specs(&settings).expect("应能生成监听项");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].address, "127.0.0.1:7004".parse().unwrap());
    }
}
