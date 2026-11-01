// SPDX-License-Identifier: GPL-3.0-only

//! 服务端内部小工具：KCP 配置、FEC 判定与地址/端口检查。

use kcptube_config::UserSettings;
use kcptube_kcp::KcpConfig;

pub(crate) fn handshake_config(mtu: i32) -> KcpConfig {
    KcpConfig {
        conv: 0,
        mtu: mtu.max(50) as usize,
        nodelay: 1,
        interval: 1,
        resend: 3,
        no_congestion_control: true,
        send_window: 32,
        receive_window: 128,
        stream_mode: false,
    }
}

pub(crate) fn data_config(settings: &UserSettings, conv: u32) -> KcpConfig {
    KcpConfig {
        conv,
        mtu: settings.kcp_mtu.max(50) as usize,
        nodelay: settings.kcp_nodelay,
        interval: settings.kcp_interval.max(1) as u32,
        resend: settings.kcp_resend.max(0) as u32,
        no_congestion_control: settings.kcp_nc != 0,
        send_window: settings.kcp_sndwnd,
        receive_window: settings.kcp_rcvwnd,
        stream_mode: false,
    }
}

/// FEC 配置：只有 k 与 r 都大于 0 时才启用。
pub(crate) fn fec_pair(settings: &UserSettings) -> Option<(u8, u8)> {
    let original = settings.fec_original_packet_count;
    let redundant = settings.fec_redundant_packet_count;
    (original > 0 && redundant > 0).then_some((original, redundant))
}

/// Windows 上 UDP 对端不可达会以「连接重置/中止」的形式返回（WSAECONNRESET）。
pub(crate) fn is_connection_reset(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
    )
}

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

    #[test]
    fn continuous_ports() {
        assert!(is_continuous(&[3000]));
        assert!(is_continuous(&[3000, 3001, 3002]));
        assert!(!is_continuous(&[3000, 3002]));
        assert!(!is_continuous(&[]));
    }
}
