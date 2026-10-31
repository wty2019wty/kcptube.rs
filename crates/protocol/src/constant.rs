// SPDX-License-Identifier: GPL-3.0-only

//! 取值与 C++ 版一致（`shares/share_defines.hpp` 的 `constant_values`
//! 与 `networks/connections.hpp` 的 `gbv_*`）。

/// 程序名
pub const APP_NAME: &str = "kcptube";

/// 版本号：Rust 版**协议兼容目标**即 C++ 版的此版本
pub const APP_VERSION: &str = "20260329";

// ---- 超时与定时 ----
/// 连接空闲超时（秒）
pub const TIMEOUT_SECONDS: u16 = 180;
/// 每次延长的秒数
pub const EXTENDS_SECONDS: u16 = 5;
/// 动态端口刷新间隔默认值（秒）
pub const DPORT_REFRESH_DEFAULT: i16 = 60;
/// 动态端口刷新最小间隔（秒）
pub const DPORT_REFRESH_MINIMAL: i16 = 20;
/// 允许的双方时间差上限（秒），对应 C++ 的 `gbv_time_gap_seconds`
pub const TIME_GAP_SECONDS: i64 = 255;
/// 保活超时（秒）= 255 >> 3
pub const KEEPALIVE_TIMEOUT: i64 = TIME_GAP_SECONDS >> 3;
/// mux 通道清理周期（秒）
pub const MUX_CHANNELS_CLEANUP: i64 = TIME_GAP_SECONDS >> 3;
/// 握手超时（秒）
pub const HANDSHAKE_TIMEOUT: usize = 30;
/// 超时清理的等待时间（秒）
pub const CLEANUP_WAITS: usize = 15;
/// KCP 清理等待（秒）
pub const KCP_CLEANUP_WAITS: i64 = 4;
/// 接收侧清理等待（秒）
pub const RECEIVER_CLEANUP_WAITS: i64 = KCP_CLEANUP_WAITS * 2;
/// 重试次数
pub const RETRY_TIMES: usize = 30;
/// 重试等待（秒）
pub const RETRY_WAITS: usize = 2;

// ---- 缓冲区 ----
/// 基础缓冲大小
pub const BUFFER_SIZE: usize = 2048;
/// 额外的端口/元数据扩展空间
pub const BUFFER_EXPAND_SIZE: usize = 128;

// ---- KCP ----
/// 发送窗口
pub const KCP_SEND_WINDOW: i32 = 1024;
/// 接收窗口
pub const KCP_RECEIVE_WINDOW: i32 = 1024;

// ---- 包层尺寸（单位：字节）----
/// 单个 UDP 包的最大长度（含头部）
pub const PACKET_LENGTH: usize = 1420;
/// 附加在加密数据尾部的 IV 校验块大小
pub const IV_CHECKSUM_BLOCK_SIZE: usize = 2;
/// 加密后预留空间（AEAD tag 等）
pub const ENCRYPTION_BLOCK_RESERVE: usize = 48;
/// `packet_layer` 头部：timestamp
pub const PACKET_LAYER_HEADER: usize = 4;
/// `packet_layer_data` 头部：timestamp + sn + sub_sn
pub const PACKET_LAYER_DATA_HEADER: usize = 9;
/// `packet_layer_fec` 头部：timestamp + sn + sub_sn + kcp_conv
pub const PACKET_LAYER_FEC_HEADER: usize = 13;
/// `fec_container` 头部：data_length
pub const FEC_CONTAINER_HEADER: usize = 2;
/// `data_layer` 头部：feature + protocol 位域
pub const DATA_LAYER_HEADER: usize = 1;
/// `mux_data_wrapper` 头部：connection_id
pub const MUX_DATA_WRAPPER_HEADER: usize = 4;
/// 自定义 IP 映射头部的最大 IP 文本长度预留
pub const IP_HEADER: usize = 36;
/// UDP 转发头部的预留
pub const UDP_HEADER: usize = 4;

/// KCP 的 MTU（扣除 IV 校验块）
pub const KCP_MTU: usize = PACKET_LENGTH - IV_CHECKSUM_BLOCK_SIZE;
/// 启用 FEC 时 KCP 可用的 MTU
pub const KCP_MTU_WITH_FEC: usize = KCP_MTU - PACKET_LAYER_FEC_HEADER;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_sizes_match_cpp() {
        // 与 C++ 中 kcp_mtu / kcp_mtu_with_fec 的计算方式一致
        assert_eq!(KCP_MTU, 1418);
        assert_eq!(KCP_MTU_WITH_FEC, 1405);
        assert_eq!(KEEPALIVE_TIMEOUT, 31);
        assert_eq!(RECEIVER_CLEANUP_WAITS, 8);
    }
}
