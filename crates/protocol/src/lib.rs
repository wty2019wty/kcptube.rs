// SPDX-License-Identifier: GPL-3.0-only

//! kcptube 线格式定义。
//!
//! 本 crate 只做「纯逻辑」的编解码，不涉及任何 IO，便于单元测试。
//!
//! **兼容性约束**：所有结构体的字节布局必须与 `legacy-cpp/src`（C++ 版
//! `app_version = 20260329`）完全一致。改动前请先用 C++ 参考实现生成测试向量。
//!
//! 分层（自外向内）：
//!
//! ```text
//! UDP datagram
//!   └─ 加密层：AEAD 密文 + 2 字节 iv_raw
//!        └─ 包层：packet_layer{ timestamp, data }            (4 字节头)
//!             ├─ FEC 冗余包：packet_layer_fec                  (13 字节头)
//!             ├─ FEC 原始包：packet_layer_data                 (9 字节头)
//!             └─ 数据层：data_layer{ feature:4 | protocol:4 }  (1 字节头)
//!                  └─ mux 包装：mux_data_wrapper              (4 字节头)
//! ```

pub mod constant;
pub mod packet;

pub use constant::*;
pub use packet::{DataLayer, Feature, ProtocolType};

/// 取当前 Unix 时间（秒）。
///
/// C++ 版 `packet::right_now()` 使用 `system_clock` 秒级时间戳，
/// 线上传输的是其低 32 位，因此这里保持一致（不要改成毫秒）。
pub fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
