// SPDX-License-Identifier: GPL-3.0-only

//! 服务端内部小工具：端口连续性判定。
//!
//! KCP 参数映射、FEC 判定与「连接重置」判定已下沉到
//! `kcptube-kcp` / `kcptube-config` / `kcptube-transport` 的公共层。

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
