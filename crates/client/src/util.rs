// SPDX-License-Identifier: GPL-3.0-only

//! 客户端内部通用工具：随机选择与 `connection_id` 分配。
//!
//! KCP 参数映射、FEC 判定、通配地址与「连接重置」判定已下沉到
//! `kcptube-kcp` / `kcptube-config` / `kcptube-transport` 的公共层。

use kcptube_transport::MuxTunnel;

/// 从列表中随机取一个元素（对应 C++ `randomly_pick_index`）。
pub(crate) fn random_pick<T>(items: &[T]) -> Option<&T> {
    if items.is_empty() {
        None
    } else {
        Some(&items[rand::random::<u32>() as usize % items.len()])
    }
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
