// SPDX-License-Identifier: GPL-3.0-only

//! 配置解析与校验的具体实现。
//!
//! 本模块逐段对应 `legacy-cpp/src/shares/configurations.cpp` 与
//! `legacy-cpp/src/shares/share_defines.cpp` 中的函数，注释里给出 C++ 函数名。
//! 除少数明显会死循环/越界的边界（例如端口范围上界为 65535）外，
//! 行为与错误文本都保持与 C++ 版一致。

use crate::{IpVersionOnly, KcpMode, RunningMode, UserSettings};
use kcptube_protocol::constant;
use std::collections::BTreeSet;

/// 解析 `mode=`（对应 C++ `parse_running_mode`）
pub(crate) fn parse_running_mode(args: &[String], settings: &mut UserSettings) -> Vec<String> {
    let mut errors = Vec::new();
    let mut count = 0u16;

    for arg in args {
        let line = arg.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(eq) = line.find('=') else {
            continue;
        };

        let name = line[..eq].trim().to_ascii_lowercase();
        let value = line[eq + 1..].trim().to_ascii_lowercase();
        if value.is_empty() {
            continue;
        }

        if name == "mode" {
            match value.as_str() {
                "server" => settings.mode = RunningMode::Server,
                "client" => settings.mode = RunningMode::Client,
                "relay" => settings.mode = RunningMode::Relay,
                _ => {
                    settings.mode = RunningMode::Unknown;
                    errors.push(format!("invalid mode: {value}"));
                }
            }
            count += 1;
        }
    }

    if count == 0 {
        errors.push("running mode is not set".to_string());
    }
    if count > 1 {
        errors.push("Too many 'mode=' in configuration file.".to_string());
    }

    errors
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Root,
    Ingress,
    Egress,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MappingSection {
    None,
    All,
    Tcp,
    Udp,
}

/// 解析除 `mode` 之外的参数（对应 C++ `parse_the_rest`）
pub(crate) fn parse_the_rest(args: &[String], root: &mut UserSettings) -> Vec<String> {
    let mut errors = Vec::new();
    let mut section = Section::Root;
    let mut mapping_section = MappingSection::None;

    for arg in args {
        let line = arg.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let eq = line.find('=');
        let name = match eq {
            Some(eq) => line[..eq].trim().to_ascii_lowercase(),
            None => line.to_ascii_lowercase(),
        };
        let has_point_to = line.contains("->");

        // ---- 分段标签 ----
        match name.as_str() {
            "[listener]" => {
                if root.mode == RunningMode::Relay {
                    if root.ingress.is_none() {
                        root.ingress = Some(Box::new(UserSettings {
                            mode: RunningMode::RelayIngress,
                            ..UserSettings::default()
                        }));
                    }
                    section = Section::Ingress;
                } else {
                    errors.push(format!("invalid section tag: {arg}"));
                }
                continue;
            }
            "[forwarder]" => {
                if root.mode == RunningMode::Relay {
                    if root.egress.is_none() {
                        root.egress = Some(Box::new(UserSettings {
                            mode: RunningMode::RelayEgress,
                            ..UserSettings::default()
                        }));
                    }
                    section = Section::Egress;
                } else {
                    errors.push(format!("invalid section tag: {arg}"));
                }
                continue;
            }
            "[custom_input]" | "[custom_input_tcp]" | "[custom_input_udp]" => {
                if root.mode == RunningMode::Client {
                    mapping_section = match name.as_str() {
                        "[custom_input]" => MappingSection::All,
                        "[custom_input_tcp]" => MappingSection::Tcp,
                        _ => MappingSection::Udp,
                    };
                    match mapping_section {
                        MappingSection::All => root.has_user_input_mappings = true,
                        MappingSection::Tcp => root.has_user_input_mappings_tcp = true,
                        MappingSection::Udp => root.has_user_input_mappings_udp = true,
                        MappingSection::None => {}
                    }
                } else {
                    errors.push(format!("invalid section tag: {arg}"));
                }
                continue;
            }
            _ => {}
        }

        // ---- `a -> b` 形式的自定义地址映射 ----
        if has_point_to {
            if mapping_section == MappingSection::None {
                errors.push(format!(
                    "invalid input: '{arg}'. Does not belongs to any custom address sections."
                ));
                continue;
            }
            let mut mapping_errors = Vec::new();
            if let Some((local, remote)) = parse_custom_input_ip(line, &mut mapping_errors) {
                let target = match mapping_section {
                    MappingSection::All => &mut root.user_input_mappings,
                    MappingSection::Tcp => &mut root.user_input_mappings_tcp,
                    MappingSection::Udp => &mut root.user_input_mappings_udp,
                    MappingSection::None => &mut root.user_input_mappings,
                };
                target.insert(local, remote);
            }
            errors.extend(mapping_errors);
            continue;
        }

        // ---- 普通 key=value ----
        let Some(eq) = eq else {
            // C++：既不是 [section] 也不含 "->"，视为未知选项
            errors.push(format!("unknow option: {arg}"));
            continue;
        };

        let original_value = line[eq + 1..].trim().to_string();
        let value = original_value.to_ascii_lowercase();
        if value.is_empty() {
            continue;
        }

        let target: &mut UserSettings = match section {
            Section::Root => root,
            Section::Ingress => match root.ingress.as_mut() {
                Some(ingress) => ingress,
                None => root,
            },
            Section::Egress => match root.egress.as_mut() {
                Some(egress) => egress,
                None => root,
            },
        };

        match name.as_str() {
            "mode" => {}
            "listen_on" => {
                if value == "{}" {
                    target.ignore_listen_address = true;
                } else {
                    // C++ 使用未转小写的原始值
                    target.listen_on = string_to_address_list(&original_value);
                }
            }
            "listen_port" => {
                if value == "{}" {
                    target.ignore_listen_port = true;
                } else {
                    target.listen_ports = string_to_port_numbers(&value, &mut errors, "listen");
                }
            }
            "dport_refresh" => match cpp_stoi(&value) {
                Some(interval) if (0..=i16::MAX as i32).contains(&interval) => {
                    target.dynamic_port_refresh = interval as i16;
                }
                Some(interval) if interval > i16::MAX as i32 => {
                    target.dynamic_port_refresh = i16::MAX;
                }
                Some(_) => {}
                None => errors.push(format!("invalid input: '{arg}', stoi")),
            },
            "destination_port" => {
                if value == "{}" {
                    target.ignore_destination_port = true;
                } else {
                    target.destination_ports =
                        string_to_port_numbers(&value, &mut errors, "destination");
                }
            }
            "destination_address" => {
                if value == "{}" {
                    target.ignore_destination_address = true;
                } else {
                    // C++ 这里使用的是转小写后的 value
                    target.destination_address_list = string_to_address_list(&value);
                }
            }
            "destination_dnstxt" => target.destination_dnstxt = value,
            "encryption_password" => target.encryption_password = original_value,
            "encryption_algorithm" => {
                target.encryption = match value.as_str() {
                    "none" => kcptube_crypto::EncryptionMode::None,
                    "xor" => kcptube_crypto::EncryptionMode::PlainXor,
                    "aes-gcm" => kcptube_crypto::EncryptionMode::AesGcm,
                    "aes-ocb" => kcptube_crypto::EncryptionMode::AesOcb,
                    "chacha20" => kcptube_crypto::EncryptionMode::Chacha20,
                    "xchacha20" => kcptube_crypto::EncryptionMode::Xchacha20,
                    _ => {
                        errors.push(format!("encryption_algorithm is incorrect: {value}"));
                        kcptube_crypto::EncryptionMode::Unknown
                    }
                };
            }
            "kcp" => {
                target.kcp_setting = match value.as_str() {
                    "manual" => KcpMode::Manual,
                    "regular1" => KcpMode::Regular1,
                    "regular2" => KcpMode::Regular2,
                    "regular3" => KcpMode::Regular3,
                    "regular4" => KcpMode::Regular4,
                    "regular5" => KcpMode::Regular5,
                    "fast1" => KcpMode::Fast1,
                    "fast2" => KcpMode::Fast2,
                    "fast3" => KcpMode::Fast3,
                    "fast4" => KcpMode::Fast4,
                    "fast5" => KcpMode::Fast5,
                    "fast6" => KcpMode::Fast6,
                    _ => {
                        errors.push(format!("invalid kcp setting: {value}"));
                        KcpMode::Unknown
                    }
                };
            }
            "mtu" => target.mtu = stoi_or_error(&value, arg, &mut errors),
            "kcp_mtu" => target.kcp_mtu = stoi_or_error(&value, arg, &mut errors),
            "kcp_nodelay" => target.kcp_nodelay = stoi_or_error(&value, arg, &mut errors),
            "kcp_interval" => target.kcp_interval = stoi_or_error(&value, arg, &mut errors),
            "kcp_resend" => target.kcp_resend = stoi_or_error(&value, arg, &mut errors),
            "kcp_nc" => {
                let yes = matches!(value.as_str(), "yes" | "true" | "1");
                target.kcp_nc = i32::from(yes);
            }
            "kcp_sndwnd" => match cpp_stoi(&value) {
                Some(wnd) if wnd >= 0 => target.kcp_sndwnd = wnd as u32,
                Some(_) => errors.push(format!("invalid kcp_sndwnd value: {value}")),
                None => errors.push(format!("invalid input: '{arg}', stoi")),
            },
            "kcp_rcvwnd" => match cpp_stoi(&value) {
                Some(wnd) if wnd >= 0 => target.kcp_rcvwnd = wnd as u32,
                Some(_) => errors.push(format!("invalid kcp_rcvwnd value: {value}")),
                None => errors.push(format!("invalid input: '{arg}', stoi")),
            },
            "udp_timeout" => match cpp_stoi(&value) {
                Some(interval) if interval <= 0 || interval > u16::MAX as i32 => {
                    target.udp_timeout = 0;
                }
                Some(interval) => target.udp_timeout = interval as u16,
                None => errors.push(format!("invalid input: '{arg}', stoi")),
            },
            "keep_alive" => match cpp_stoi(&value) {
                Some(interval) if interval <= 0 => target.keep_alive = 0,
                Some(interval) if interval < u16::MAX as i32 => {
                    target.keep_alive = interval as u16;
                }
                Some(_) => target.keep_alive = u16::MAX,
                None => errors.push(format!("invalid input: '{arg}', stoi")),
            },
            "mux_tunnels" => match cpp_stoi(&value) {
                Some(tunnels) if tunnels <= 0 => target.mux_tunnels = 0,
                Some(tunnels) if tunnels < u16::MAX as i32 => target.mux_tunnels = tunnels as u16,
                Some(_) => target.mux_tunnels = u16::MAX,
                None => errors.push(format!("invalid input: '{arg}', stoi")),
            },
            "stun_server" => target.stun_server = original_value,
            "update_ipv4" => target.update_ipv4_path = original_value,
            "update_ipv6" => target.update_ipv6_path = original_value,
            "outbound_bandwidth" => {
                target.outbound_bandwidth = bandwidth_from_string(&original_value, &mut errors);
            }
            "inbound_bandwidth" => {
                target.inbound_bandwidth = bandwidth_from_string(&original_value, &mut errors);
            }
            "log_path" => target.log_directory = original_value.into(),
            "ipv4_only" => {
                // C++ 无条件置位 ipv4 位（不判断 yes）
                target.ip_version_only = match target.ip_version_only {
                    IpVersionOnly::Ipv6 | IpVersionOnly::Both => IpVersionOnly::Both,
                    _ => IpVersionOnly::Ipv4,
                };
            }
            "ipv6_only" => {
                // C++ 无条件赋值为 ipv6
                target.ip_version_only = IpVersionOnly::Ipv6;
            }
            "blast" => {
                target.blast = matches!(value.as_str(), "yes" | "true" | "1");
            }
            "fec" => parse_fec(&value, arg, target, &mut errors),
            "fib_ingress" => match cpp_stoi(&value) {
                Some(fib) if fib <= 0 => target.fib_ingress = 0,
                Some(fib) if fib < u16::MAX as i32 => target.fib_ingress = fib,
                Some(_) => target.fib_ingress = u16::MAX as i32,
                None => errors.push(format!("invalid input: '{arg}', stoi")),
            },
            "fib_egress" => match cpp_stoi(&value) {
                Some(fib) if fib <= 0 => target.fib_egress = 0,
                Some(fib) if fib < u16::MAX as i32 => target.fib_egress = fib,
                Some(_) => target.fib_egress = u16::MAX as i32,
                None => errors.push(format!("invalid input: '{arg}', stoi")),
            },
            _ => errors.push(format!("unknow option: {arg}")),
        }
    }

    errors
}

/// 对应 C++ `std::stoi`：允许前导空白与正负号，允许尾部多余字符，
/// 超出 `int` 范围或没有数字时视为失败。
fn cpp_stoi(value: &str) -> Option<i32> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }

    let mut negative = false;
    if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
        negative = bytes[index] == b'-';
        index += 1;
    }

    let start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == start {
        return None;
    }

    let digits = &value[start..index];
    let mut result: i64 = 0;
    for byte in digits.bytes() {
        result = result
            .saturating_mul(10)
            .saturating_add(i64::from(byte - b'0'));
        if result > i32::MAX as i64 + 1 {
            return None;
        }
    }
    if negative {
        result = -result;
    }
    if (i32::MIN as i64..=i32::MAX as i64).contains(&result) {
        Some(result as i32)
    } else {
        None
    }
}

fn stoi_or_error(value: &str, arg: &str, errors: &mut Vec<String>) -> i32 {
    match cpp_stoi(value) {
        Some(value) => value,
        None => {
            errors.push(format!("invalid input: '{arg}', stoi"));
            0
        }
    }
}

fn parse_fec(value: &str, arg: &str, target: &mut UserSettings, errors: &mut Vec<String>) {
    let Some(pos) = value.find(':') else {
        errors.push(format!("invalid fec format: {value}"));
        return;
    };

    let data_part = value[..pos].trim();
    let redundant_part = value[pos + 1..].trim();
    if data_part.is_empty() || redundant_part.is_empty() {
        errors.push(format!("invalid fec setting: {value}"));
        return;
    }

    let Some(data_number) = cpp_stoi(data_part) else {
        errors.push(format!("invalid input: '{arg}', stoi"));
        return;
    };
    let Some(redundant_number) = cpp_stoi(redundant_part) else {
        errors.push(format!("invalid input: '{arg}', stoi"));
        return;
    };

    if data_number > 0 && data_number <= u8::MAX as i32 {
        target.fec_original_packet_count = data_number as u8;
    }
    if redundant_number > 0 && redundant_number <= u8::MAX as i32 {
        target.fec_redundant_packet_count = redundant_number as u8;
    }

    if data_number + redundant_number > u8::MAX as i32 {
        errors.push(format!(
            "the sum of fec value is too large: {} ({arg})",
            data_number + redundant_number
        ));
    }

    if target.fec_original_packet_count == 0 || target.fec_redundant_packet_count == 0 {
        target.fec_original_packet_count = 0;
        target.fec_redundant_packet_count = 0;
    }
}

/// 对应 C++ `split_address`：拆出 `地址:端口` 并校验。
fn split_address(input: &str) -> (String, String, Vec<String>) {
    let mut errors = Vec::new();
    let Some(colon) = input.rfind(':') else {
        return (String::new(), String::new(), errors);
    };

    let mut address = input[..colon].trim().to_string();
    let mut port = input[colon + 1..].trim().to_string();

    let correct_port = cpp_stoi(&port)
        .map(|value| value > 0 && value < 65536)
        .unwrap_or(false);

    let mut correct_address = false;
    if address.is_empty() {
        correct_address = true;
    } else if address.starts_with('[') || address.ends_with(']') {
        if address.starts_with('[') && address.ends_with(']') {
            let inner = &address[1..address.len() - 1];
            correct_address = inner.parse::<std::net::Ipv6Addr>().is_ok();
            address = inner.to_string();
        }
    } else {
        correct_address = address.parse::<std::net::Ipv4Addr>().is_ok();
    }

    if !correct_address {
        let found_colon = address.contains(':');
        if let Some(dot) = address.rfind('.') {
            if !found_colon && dot > 0 {
                let suffix = &address[dot + 1..];
                if suffix.bytes().any(|b| b.is_ascii_digit()) {
                    address.clear();
                    errors.push("Address Incorrect".to_string());
                } else {
                    // 末尾是字母，视为域名
                    correct_address = true;
                }
            }
        }
        let _ = correct_address;
    }

    if !correct_port {
        port.clear();
        errors.push("Port Number Incorrect".to_string());
    }

    (address, port, errors)
}

/// 对应 C++ `parse_custom_input_ip`
fn parse_custom_input_ip(
    line: &str,
    errors: &mut Vec<String>,
) -> Option<((String, u16), (String, u16))> {
    let point_to = line.find("->")?;
    let local_address = line[..point_to].trim();
    let remote_address = line[point_to + 2..].trim();

    let (local_ip, local_port, local_errors) = split_address(local_address);
    let (remote_ip, remote_port, remote_errors) = split_address(remote_address);

    if !local_errors.is_empty() || !remote_errors.is_empty() || remote_ip.is_empty() {
        if !local_errors.is_empty() {
            errors.push(format!("'{line}',{}", local_errors.concat()));
        }
        if !remote_errors.is_empty() {
            errors.push(format!("'{line}',{}", remote_errors.concat()));
        }
        if remote_ip.is_empty() {
            errors.push(format!("'{line}' Remote Address can't be empty"));
        }
        return None;
    }

    let Some(local_port_number) = cpp_stoi(&local_port) else {
        errors.push(format!("invalid input: '{line}', stoi"));
        return None;
    };
    let Some(remote_port_number) = cpp_stoi(&remote_port) else {
        errors.push(format!("invalid input: '{line}', stoi"));
        return None;
    };
    Some((
        (local_ip, local_port_number as u16),
        (remote_ip, remote_port_number as u16),
    ))
}

/// 对应 C++ `port_range_to_vector`
pub(crate) fn port_range_to_vector(
    input: &str,
    errors: &mut Vec<String>,
    acting_role: &str,
) -> Vec<u16> {
    let mut numbers = BTreeSet::new();

    let Some(pos) = input.find('-') else {
        let port_str = input.trim();
        match cpp_stoi(port_str) {
            Some(port) if port > 0 && port <= u16::MAX as i32 => {
                numbers.insert(port as u16);
            }
            _ => errors.push(format!("invalid {acting_role}_port number: {port_str}")),
        }
        return numbers.into_iter().collect();
    };

    let start = input[..pos].trim();
    let end = input[pos + 1..].trim();
    if start.is_empty() || end.is_empty() {
        errors.push(format!("invalid {acting_role}_port range: {input}"));
        return Vec::new();
    }

    let mut temp_start = 0u16;
    let mut temp_end = 0u16;
    match cpp_stoi(start) {
        Some(port) if port > 0 && port <= u16::MAX as i32 => temp_start = port as u16,
        _ => errors.push(format!("invalid {acting_role}_port_start number: {start}")),
    }
    match cpp_stoi(end) {
        Some(port) if port > 0 && port <= u16::MAX as i32 => temp_end = port as u16,
        _ => errors.push(format!("invalid {acting_role}_port_end number: {end}")),
    }

    if temp_start >= temp_end {
        errors.push(format!("invalid port range: {start}-{end}"));
        return Vec::new();
    }

    for port in u32::from(temp_start)..=u32::from(temp_end) {
        if let Ok(port) = u16::try_from(port) {
            numbers.insert(port);
        }
    }
    numbers.into_iter().collect()
}

/// 对应 C++ `string_to_port_numbers`
pub(crate) fn string_to_port_numbers(
    input: &str,
    errors: &mut Vec<String>,
    acting_role: &str,
) -> Vec<u16> {
    let mut numbers = BTreeSet::new();
    if !input.contains(',') {
        return port_range_to_vector(input, errors, acting_role);
    }

    let mut parts: Vec<&str> = input.split(',').collect();
    // std::getline 不会返回末尾的空字段
    if parts.last().is_some_and(|part| part.is_empty()) {
        parts.pop();
    }
    for part in parts {
        for port in port_range_to_vector(part.trim(), errors, acting_role) {
            numbers.insert(port);
        }
    }
    numbers.into_iter().collect()
}

/// 对应 C++ `string_to_address_list`
pub(crate) fn string_to_address_list(input: &str) -> Vec<String> {
    if !input.contains(',') {
        return vec![input.trim().to_string()];
    }

    let mut parts: Vec<&str> = input.split(',').collect();
    if parts.last().is_some_and(|part| part.is_empty()) {
        parts.pop();
    }
    let addresses: BTreeSet<String> = parts.iter().map(|part| part.trim().to_string()).collect();
    addresses.into_iter().collect()
}

/// 对应 C++ `bandwidth_from_string`
pub(crate) fn bandwidth_from_string(bandwidth: &str, errors: &mut Vec<String>) -> u64 {
    if bandwidth.is_empty() {
        return 0;
    }

    const KILO: u64 = 1000;
    const KIBI: u64 = 1024;
    let mut expand: u64 = 1;
    let unit = bandwidth.as_bytes().last().copied().unwrap_or(b'0');

    match unit {
        b'K' => expand = KIBI,
        b'k' => expand = KILO,
        b'M' => expand = KIBI * KIBI,
        b'm' => expand = KILO * KILO,
        b'G' => expand = KIBI * KIBI * KIBI,
        b'g' => expand = KILO * KILO * KILO,
        b'0'..=b'9' => {}
        _ => errors.push("Unknow bandwidth unit".to_string()),
    }

    // C++ 无论是否识别单位都会 pop_back 掉最后一个字符
    let number_bytes = &bandwidth.as_bytes()[..bandwidth.len() - 1];
    let number_part = std::str::from_utf8(number_bytes).unwrap_or("");
    if number_part.is_empty() {
        return 0;
    }

    match cpp_stoi(number_part.trim()) {
        Some(number) if number >= 0 => (number as u64).saturating_mul(expand) / 8,
        Some(_) => {
            errors.push("bandwidth convertion failed".to_string());
            0
        }
        None => {
            errors.push("bandwidth convertion failed".to_string());
            0
        }
    }
}

/// 对应 C++ `check_settings`
pub(crate) fn check_settings(settings: &mut UserSettings, errors: &mut Vec<String>) {
    if settings.mode == RunningMode::Relay {
        if settings.ingress.is_none() && settings.egress.is_none() {
            errors.push("[listener] and [forwarder] are missing".to_string());
        }
        if settings.ingress.is_some() || settings.egress.is_some() {
            if settings.ingress.is_none() {
                errors.push("[listener] is missing".to_string());
            }
            if settings.egress.is_none() {
                errors.push("[forwarder] is missing".to_string());
            }
        }
        if settings.mux_tunnels > 0 {
            errors.push("mux_tunnels should not be set".to_string());
        }
    }

    // copy_settings(inner, outer)：把根配置里显式设置的公共项下沉到 relay 分段
    let outer = settings.clone();
    if let Some(ingress) = settings.ingress.as_mut() {
        copy_settings(ingress, &outer);
    }
    if let Some(egress) = settings.egress.as_mut() {
        copy_settings(egress, &outer);
    }

    verify_kcp_settings(settings, errors);

    if settings.dynamic_port_refresh < 0 {
        settings.dynamic_port_refresh = constant::DPORT_REFRESH_DEFAULT;
    } else if settings.dynamic_port_refresh == 0 {
        // 显式关闭端口跳跃
    } else if settings.dynamic_port_refresh < constant::DPORT_REFRESH_MINIMAL {
        settings.dynamic_port_refresh = constant::DPORT_REFRESH_MINIMAL;
    }

    if settings.udp_timeout == 0 {
        settings.udp_timeout = constant::TIMEOUT_SECONDS;
    }

    if !matches!(
        settings.encryption,
        kcptube_crypto::EncryptionMode::Empty
            | kcptube_crypto::EncryptionMode::Unknown
            | kcptube_crypto::EncryptionMode::None
            | kcptube_crypto::EncryptionMode::PlainXor
    ) && settings.encryption_password.is_empty()
    {
        errors.push("encryption_password is not set".to_string());
    }

    if settings.mode == RunningMode::Client {
        if settings.ignore_listen_port || settings.ignore_listen_address {
            settings.listen_ports.clear();
            settings.listen_on.clear();

            if !settings.has_user_input_mappings
                && !settings.has_user_input_mappings_tcp
                && !settings.has_user_input_mappings_udp
            {
                errors.push("custom address section tag is empty".to_string());
            }
        } else if settings.listen_ports.is_empty() {
            errors.push("listen_port is not set".to_string());
        }

        if settings.ignore_destination_address {
            errors.push("destination_address can't be ignored".to_string());
        }
        if settings.ignore_destination_port {
            errors.push("destination_port can't be ignored".to_string());
        }

        verify_client_destination(settings, errors);
    }

    if settings.mode == RunningMode::Server {
        if settings.ignore_listen_address {
            errors.push(
                "if listen_address should be ignored, please delete the whole line of listen_address"
                    .to_string(),
            );
        }
        if settings.ignore_listen_port {
            errors.push("listen_port can't be ignored".to_string());
        }

        verify_server_listen_port(settings, errors);

        if !settings.ignore_destination_address && !settings.ignore_destination_port {
            if settings.destination_ports.is_empty() {
                errors.push("destination_port is not set".to_string());
            }
            if settings.destination_ports.len() > 1 {
                errors.push("too many destination_port".to_string());
            }
            if settings.destination_address_list.len() != 1 {
                errors.push("invalid destination_address setting".to_string());
            }
        }

        if settings.mux_tunnels > 0 {
            errors.push("mux_tunnels should not be set".to_string());
        }
    }

    if settings.mode == RunningMode::RelayIngress {
        verify_server_listen_port(settings, errors);
    }
    if settings.mode == RunningMode::RelayEgress {
        verify_client_destination(settings, errors);
    }

    if !settings.stun_server.is_empty() && settings.mode != RunningMode::Relay {
        if settings.listen_ports.len() > 1 {
            errors.push("do not specify multiple listen ports when STUN Server is set".to_string());
        }
        if settings.listen_on.len() > 1 {
            errors.push(
                "do not specify multiple listen addresses when STUN Server is set".to_string(),
            );
        }
    }

    if !settings.log_directory.as_os_str().is_empty()
        && settings.mode != RunningMode::RelayIngress
        && settings.mode != RunningMode::RelayEgress
    {
        match std::fs::metadata(&settings.log_directory) {
            Ok(metadata) if metadata.is_dir() => {
                let filename = match settings.mode {
                    RunningMode::Client => "client_output.log",
                    RunningMode::Server => "server_output.log",
                    RunningMode::Relay => "relay_output.log",
                    _ => "log_output.log",
                };
                settings.log_ip_address = settings.log_directory.join("ip_address.log");
                settings.log_messages = settings.log_directory.join(filename);
            }
            Ok(_) => errors.push("Log Path is not directory".to_string()),
            Err(_) => errors.push("Log Path does not exist".to_string()),
        }
    }

    if settings.ip_version_only == IpVersionOnly::Both {
        errors.push("Both ipv4_only and ipv6_only are set as true".to_string());
    }

    if errors.is_empty() {
        if let Some(ingress) = settings.ingress.as_mut() {
            check_settings(ingress, errors);
            if u32::from(ingress.udp_timeout) + u32::from(constant::EXTENDS_SECONDS)
                <= u32::from(u16::MAX)
            {
                ingress.udp_timeout += constant::EXTENDS_SECONDS;
            }
        }
    }

    if errors.is_empty() {
        if let Some(egress) = settings.egress.as_mut() {
            check_settings(egress, errors);
            if u32::from(egress.udp_timeout) + u32::from(constant::EXTENDS_SECONDS)
                <= u32::from(u16::MAX)
            {
                egress.udp_timeout += constant::EXTENDS_SECONDS;
            }
        }
    }
}

/// 对应 C++ `copy_settings`：仅当外层显式设置时才覆盖内层
fn copy_settings(inner: &mut UserSettings, outer: &UserSettings) {
    if outer.mtu > 0 {
        inner.mtu = outer.mtu;
    }
    if outer.fec_original_packet_count > 0 {
        inner.fec_original_packet_count = outer.fec_original_packet_count;
    }
    if outer.fec_redundant_packet_count > 0 {
        inner.fec_redundant_packet_count = outer.fec_redundant_packet_count;
    }
    if outer.kcp_setting != KcpMode::Unknown {
        inner.kcp_setting = outer.kcp_setting;
    }
    if outer.kcp_rcvwnd > 0 {
        inner.kcp_rcvwnd = outer.kcp_rcvwnd;
    }
    if outer.kcp_sndwnd > 0 {
        inner.kcp_sndwnd = outer.kcp_sndwnd;
    }
    if outer.outbound_bandwidth > 0 {
        inner.outbound_bandwidth = outer.outbound_bandwidth;
    }
    if outer.inbound_bandwidth > 0 {
        inner.inbound_bandwidth = outer.inbound_bandwidth;
    }
    if !matches!(
        outer.encryption,
        kcptube_crypto::EncryptionMode::Unknown
            | kcptube_crypto::EncryptionMode::Empty
            | kcptube_crypto::EncryptionMode::None
    ) {
        inner.encryption = outer.encryption;
    }
    if !outer.encryption_password.is_empty() {
        inner.encryption_password = outer.encryption_password.clone();
    }
    if outer.udp_timeout > 0 {
        inner.udp_timeout = outer.udp_timeout;
    }
    if outer.keep_alive > 0 {
        inner.keep_alive = outer.keep_alive;
    }
    if outer.ip_version_only.is_set() {
        inner.ip_version_only = outer.ip_version_only;
    }
    if !outer.update_ipv4_path.is_empty() {
        inner.update_ipv4_path = outer.update_ipv4_path.clone();
    }
    if !outer.update_ipv6_path.is_empty() {
        inner.update_ipv6_path = outer.update_ipv6_path.clone();
    }
    if outer.blast {
        inner.blast = outer.blast;
    }
    if outer.fib_ingress > 0 {
        inner.fib_ingress = outer.fib_ingress;
    }
    if outer.fib_egress > 0 {
        inner.fib_egress = outer.fib_egress;
    }
}

/// 对应 C++ `verify_kcp_settings`
fn verify_kcp_settings(settings: &mut UserSettings, errors: &mut Vec<String>) {
    match settings.kcp_setting {
        KcpMode::Manual => {
            if settings.kcp_nodelay < 0 {
                errors.push("kcp_nodelay not set".to_string());
            }
            if settings.kcp_interval < 0 {
                errors.push("kcp_interval not set".to_string());
            }
            if settings.kcp_resend < 0 {
                errors.push("kcp_resend not set".to_string());
            }
            if settings.kcp_nc < 0 {
                errors.push("kcp_nc not set".to_string());
            }
            if settings.kcp_sndwnd == 0 {
                settings.kcp_sndwnd = (constant::KCP_SEND_WINDOW * 2) as u32;
            }
            if settings.kcp_rcvwnd == 0 {
                settings.kcp_rcvwnd = (constant::KCP_RECEIVE_WINDOW * 2) as u32;
            }
        }
        KcpMode::Fast1
        | KcpMode::Fast2
        | KcpMode::Fast3
        | KcpMode::Fast4
        | KcpMode::Fast5
        | KcpMode::Fast6 => {
            let (nodelay, interval, resend) = match settings.kcp_setting {
                KcpMode::Fast1 => (1, 0, 2),
                KcpMode::Fast2 => (2, 0, 2),
                KcpMode::Fast3 => (1, 1, 3),
                KcpMode::Fast4 => (2, 1, 3),
                KcpMode::Fast5 => (1, 1, 4),
                _ => (2, 1, 4),
            };
            settings.kcp_nodelay = nodelay;
            settings.kcp_interval = interval;
            settings.kcp_resend = resend;
            settings.kcp_nc = 1;
            if settings.kcp_sndwnd == 0 {
                settings.kcp_sndwnd = (constant::KCP_SEND_WINDOW * 2) as u32;
            }
            if settings.kcp_rcvwnd == 0 {
                settings.kcp_rcvwnd = (constant::KCP_RECEIVE_WINDOW * 2) as u32;
            }
        }
        KcpMode::Regular1 | KcpMode::Regular2 | KcpMode::Regular4 => {
            let (nodelay, interval) = match settings.kcp_setting {
                KcpMode::Regular1 => (1, 1),
                KcpMode::Regular2 => (2, 1),
                _ => (0, 15),
            };
            settings.kcp_nodelay = nodelay;
            settings.kcp_interval = interval;
            settings.kcp_resend = if settings.kcp_setting == KcpMode::Regular4 {
                2
            } else {
                5
            };
            settings.kcp_nc = 1;
            if settings.kcp_sndwnd == 0 {
                settings.kcp_sndwnd = constant::KCP_SEND_WINDOW as u32;
            }
            if settings.kcp_rcvwnd == 0 {
                settings.kcp_rcvwnd = constant::KCP_RECEIVE_WINDOW as u32;
            }
        }
        KcpMode::Regular3 => {
            settings.kcp_nodelay = 0;
            settings.kcp_interval = 1;
            settings.kcp_resend = 2;
            settings.kcp_nc = 1;
            if settings.kcp_sndwnd == 0 {
                settings.kcp_sndwnd = constant::KCP_SEND_WINDOW as u32;
            }
            if settings.kcp_rcvwnd == 0 {
                settings.kcp_rcvwnd = constant::KCP_RECEIVE_WINDOW as u32;
            }
        }
        KcpMode::Regular5 | KcpMode::Unknown => {
            settings.kcp_nodelay = 0;
            settings.kcp_interval = 30;
            settings.kcp_resend = 2;
            settings.kcp_nc = 1;
            if settings.kcp_sndwnd == 0 {
                settings.kcp_sndwnd = constant::KCP_SEND_WINDOW as u32;
            }
            if settings.kcp_rcvwnd == 0 {
                settings.kcp_rcvwnd = constant::KCP_RECEIVE_WINDOW as u32;
            }
        }
    }

    if settings.mtu > 0 {
        let outer_verify_size = if settings.encryption_password.is_empty() {
            constant::IV_CHECKSUM_BLOCK_SIZE as i32
        } else {
            constant::ENCRYPTION_BLOCK_RESERVE as i32
        };

        let mut headers_length =
            (constant::IP_HEADER + constant::UDP_HEADER + constant::DATA_LAYER_HEADER) as i32;

        if settings.fec_original_packet_count > 0 && settings.fec_redundant_packet_count > 0 {
            headers_length +=
                (constant::PACKET_LAYER_FEC_HEADER + constant::FEC_CONTAINER_HEADER) as i32;
        } else {
            headers_length = constant::PACKET_LAYER_HEADER as i32;
        }

        if settings.mux_tunnels > 0 {
            headers_length += constant::MUX_DATA_WRAPPER_HEADER as i32;
        }

        settings.kcp_mtu = settings.mtu - outer_verify_size - headers_length;
    }

    if settings.kcp_mtu <= 0 {
        settings.kcp_mtu =
            if settings.fec_original_packet_count > 0 && settings.fec_redundant_packet_count > 0 {
                constant::KCP_MTU_WITH_FEC as i32
            } else {
                constant::KCP_MTU as i32
            };
    }
}

fn verify_server_listen_port(settings: &UserSettings, errors: &mut Vec<String>) {
    if settings.listen_ports.is_empty() {
        errors.push("listen_port is not set".to_string());
    }
}

fn verify_client_destination(settings: &UserSettings, errors: &mut Vec<String>) {
    if settings.destination_dnstxt.is_empty() {
        if settings.destination_ports.is_empty() {
            errors.push("destination port setting incorrect".to_string());
        }
        if settings.destination_address_list.is_empty() {
            errors.push("invalid destination_address setting".to_string());
        }
    } else {
        if !settings.destination_address_list.is_empty() {
            errors.push("destination_address: DNS TXT setting exists".to_string());
        }
        if !settings.destination_ports.is_empty() {
            errors.push("destination_port: DNS TXT setting exists".to_string());
        }
    }
}
