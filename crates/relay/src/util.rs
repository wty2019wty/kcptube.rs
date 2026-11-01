// SPDX-License-Identifier: GPL-3.0-only

//! relay 的配置映射与小工具。

use kcptube_config::UserSettings;
use std::net::{IpAddr, SocketAddr};

/// 枚举 ingress 需要绑定的全部 UDP 地址（`listen_on` × `listen_ports`）。
///
/// 与 C++ `relay.cpp:55`–`93` 的差异：C++ 在未指定 IP 版本时会把 IPv4 地址
/// 映射成 v4-mapped IPv6 再绑定 v6 socket；Rust 的 socket 有明确地址族，
/// 直接按配置的地址族绑定更稳妥（Windows 默认 `IPV6_V6ONLY=1` 时，
/// 绑定 `::ffff:a.b.c.d` 会失败）。`listen_on` 为空时仍按 `ip_version_only`
/// 选择通配地址，与 server 模式一致。
pub(crate) fn listen_addresses(settings: &UserSettings) -> Vec<SocketAddr> {
    let ips: Vec<IpAddr> = if settings.listen_on.is_empty() {
        vec![settings.ip_version_only.wildcard_ip()]
    } else {
        settings
            .listen_on
            .iter()
            .filter_map(|text| text.parse::<IpAddr>().ok())
            .collect()
    };

    let mut addresses = Vec::new();
    for ip in ips {
        for port in &settings.listen_ports {
            addresses.push(SocketAddr::new(ip, *port));
        }
    }
    addresses
}

/// 从列表中随机取一个元素（对应 C++ `randomly_pick_index`）。
pub(crate) fn random_pick<T>(items: &[T]) -> Option<&T> {
    if items.is_empty() {
        None
    } else {
        Some(&items[rand::random::<u32>() as usize % items.len()])
    }
}

/// 端口列表是否连续（用于判断能否把范围透传给客户端）。
pub(crate) fn is_continuous(ports: &[u16]) -> bool {
    if ports.is_empty() {
        return false;
    }
    ports
        .windows(2)
        .all(|pair| pair[1] == pair[0].saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kcptube_config::IpVersionOnly;

    fn settings() -> UserSettings {
        UserSettings {
            listen_ports: vec![3000, 3001],
            ..Default::default()
        }
    }

    #[test]
    fn empty_listen_on_uses_wildcard() {
        let mut settings = settings();
        settings.ip_version_only = IpVersionOnly::Ipv6;
        let addresses = listen_addresses(&settings);
        assert_eq!(addresses.len(), 2);
        assert!(addresses.iter().all(|address| address.is_ipv6()));
    }

    #[test]
    fn explicit_address_keeps_its_family() {
        let mut settings = settings();
        settings.listen_on = vec!["0.0.0.0".to_string()];
        let addresses = listen_addresses(&settings);
        assert_eq!(addresses.len(), 2);
        assert!(addresses.iter().all(|address| address.is_ipv4()));
        assert_eq!(addresses[0].port(), 3000);
        assert_eq!(addresses[1].port(), 3001);
    }

    #[test]
    fn ipv4_only_keeps_ipv4() {
        let mut settings = settings();
        settings.listen_on = vec!["127.0.0.1".to_string()];
        settings.ip_version_only = IpVersionOnly::Ipv4;
        let addresses = listen_addresses(&settings);
        assert!(addresses.iter().all(|address| address.is_ipv4()));
    }

    #[test]
    fn random_pick_handles_empty() {
        let empty: [u8; 0] = [];
        assert!(random_pick(&empty).is_none());
        assert_eq!(random_pick(&[7u8]), Some(&7));
    }
}
