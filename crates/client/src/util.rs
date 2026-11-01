// SPDX-License-Identifier: GPL-3.0-only

//! 客户端内部通用工具：随机选择、地址处理、KCP 配置与 `connection_id` 分配。

use kcptube_config::UserSettings;
use kcptube_kcp::KcpConfig;
use kcptube_transport::MuxTunnel;
use std::net::SocketAddr;

/// 从列表中随机取一个元素（对应 C++ `randomly_pick_index`）。
pub(crate) fn random_pick<T>(items: &[T]) -> Option<&T> {
    if items.is_empty() {
        None
    } else {
        Some(&items[rand::random::<u32>() as usize % items.len()])
    }
}

/// Windows 上 UDP 对端不可达会以「连接重置/中止」的形式返回（WSAECONNRESET）。
pub(crate) fn is_connection_reset(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
    )
}

/// 根据对端地址族选择本地通配地址。
pub(crate) fn wildcard_addr(peer: SocketAddr) -> SocketAddr {
    if peer.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    }
}

/// FEC 配置：只有 k 与 r 都大于 0 时才启用。
pub(crate) fn fec_pair(settings: &UserSettings) -> Option<(u8, u8)> {
    let original = settings.fec_original_packet_count;
    let redundant = settings.fec_redundant_packet_count;
    (original > 0 && redundant > 0).then_some((original, redundant))
}

/// 分配一个未占用的 `connection_id`（对应 C++ `generate_random_number<uint32_t>`）。
pub(crate) fn allocate_connection_id(tunnel: &MuxTunnel) -> u32 {
    loop {
        let id = rand::random::<u32>();
        if tunnel.record(id).is_none() {
            return id;
        }
    }
}

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
