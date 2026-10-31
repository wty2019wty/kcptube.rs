// SPDX-License-Identifier: GPL-3.0-only

//! 日志输出（对应 C++ `shares/share_defines.cpp` 中的日志函数）。
//!
//! C++ 版把运行期信息分成三类文件，路径全部来自配置：
//!
//! | 配置项 | 写入方式 | 内容 |
//! |---|---|---|
//! | `log_messages`（`log_path` 下的 `client_output.log` / `server_output.log`） | **追加** | 运行期事件（握手失败、解密失败等） |
//! | `log_status`（`log_path` 下的 `<配置文件名>_status.log`） | **覆盖** | 每 60 秒刷新一次的状态摘要 |
//! | `log_ip_address`（`log_path` 下的 `ip_address.log`） | **覆盖** | 当前出口 IP（STUN / DDNS） |
//!
//! 路径为空时静默跳过——与 C++ 版 `if (log_file.empty()) return;` 一致。

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

/// 状态摘要的刷新间隔（秒），对应 C++ `gbv_logging_gap`
pub const LOGGING_GAP_SECONDS: u64 = 60;

/// 本地时间，格式与 C++ `time_to_string()`（`std::put_time("%F %T %z")`）一致，
/// 例如 `2026-10-31 10:21:40 +0800`。
pub fn time_to_string() -> String {
    chrono::Local::now().format("%F %T %z").to_string()
}

/// 带方括号前缀的时间戳，对应 C++ `time_to_string_with_square_brackets()`。
pub fn time_string_with_brackets() -> String {
    format!("[{}] ", time_to_string())
}

/// 向 `log_messages` 追加一条事件。
pub fn print_message_to_file(path: &Path, message: &str) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(message.as_bytes())
}

/// 覆盖写 `log_status`。
pub fn print_status_to_file(path: &Path, message: &str) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(message.as_bytes())
}

/// 覆盖写 `log_ip_address`。
pub fn print_ip_to_file(path: &Path, message: &str) -> io::Result<()> {
    print_status_to_file(path, message)
}

/// 把「统计周期内的字节数」转换成 C++ `to_speed_unit()` 的文本形式。
///
/// 复刻了 C++ 的分档逻辑（含 `value_per_second == 0` 时输出总量而非速度这一特殊行为）。
pub fn to_speed_unit(value: u64, duration_seconds: u64) -> String {
    if value == 0 {
        return "0 Byte/s".to_string();
    }

    let duration_seconds = duration_seconds.max(1);
    let value_per_second = value / duration_seconds;
    let length = if value_per_second == 0 {
        0
    } else {
        (value_per_second as f64).log10() as i64
    };

    if length == 0 {
        return format!("{value} Bytes / {duration_seconds} seconds");
    }
    if length <= 3 {
        return format!("{value_per_second} Bytes/s");
    }
    if length <= 6 {
        return format!("{} KiB/s", value_per_second / 1024);
    }
    if length <= 9 {
        return format!("{} MiB/s", value_per_second / 1024 / 1024);
    }
    format!("{} GiB/s", value_per_second / 1024 / 1024 / 1024)
}

/// 运行期流量计数器（对应 C++ `status_counters`）。
///
/// 每个周期读取并清零，因此状态摘要展示的是「过去 60 秒」的数值。
#[derive(Default)]
pub struct StatusCounters {
    pub ingress_raw_traffic: std::sync::atomic::AtomicU64,
    pub ingress_inner_traffic: std::sync::atomic::AtomicU64,
    pub egress_inner_traffic: std::sync::atomic::AtomicU64,
    pub egress_raw_traffic: std::sync::atomic::AtomicU64,
    pub fec_recovery_count: std::sync::atomic::AtomicU64,
}

impl StatusCounters {
    pub fn add_ingress_raw(&self, value: usize) {
        use std::sync::atomic::Ordering;
        self.ingress_raw_traffic
            .fetch_add(value as u64, Ordering::Relaxed);
    }

    pub fn add_ingress_inner(&self, value: usize) {
        use std::sync::atomic::Ordering;
        self.ingress_inner_traffic
            .fetch_add(value as u64, Ordering::Relaxed);
    }

    pub fn add_egress_inner(&self, value: usize) {
        use std::sync::atomic::Ordering;
        self.egress_inner_traffic
            .fetch_add(value as u64, Ordering::Relaxed);
    }

    pub fn add_egress_raw(&self, value: usize) {
        use std::sync::atomic::Ordering;
        self.egress_raw_traffic
            .fetch_add(value as u64, Ordering::Relaxed);
    }

    /// 取出并清零一类计数
    pub fn take(counter: &std::sync::atomic::AtomicU64) -> u64 {
        counter.swap(0, std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_unit_matches_cpp_buckets() {
        assert_eq!(to_speed_unit(0, 60), "0 Byte/s");
        // 低于每秒 1 字节时 C++ 输出总量
        assert_eq!(to_speed_unit(100, 60), "100 Bytes / 60 seconds");
        assert_eq!(to_speed_unit(60, 60), "60 Bytes / 60 seconds");
        // 每秒 100 字节
        assert_eq!(to_speed_unit(6000, 60), "100 Bytes/s");
        // 每秒 10240 字节
        assert_eq!(to_speed_unit(614_400, 60), "10 KiB/s");
        // C++ 以 log10 的整数部分分档，1 MiB/s 仍落在 KiB 档（log10≈6.02 → 6）
        assert_eq!(to_speed_unit(1024 * 1024 * 60, 60), "1024 KiB/s");
        // 10^10 B/s 才进入 GiB 档
        assert_eq!(to_speed_unit(10_000_000_000 * 60, 60), "9 GiB/s");
    }

    #[test]
    fn zero_duration_is_treated_as_one() {
        assert_eq!(to_speed_unit(6000, 0), "6000 Bytes/s");
    }

    #[test]
    fn empty_path_writes_nothing() {
        assert!(print_message_to_file(Path::new(""), "x").is_ok());
        assert!(print_status_to_file(Path::new(""), "x").is_ok());
        assert!(print_ip_to_file(Path::new(""), "x").is_ok());
    }

    #[test]
    fn message_appends_and_status_truncates() {
        let dir = std::env::temp_dir().join(format!("kcptube-logging-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        let message_path = dir.join("messages.log");
        let status_path = dir.join("status.log");

        print_message_to_file(&message_path, "first\n").expect("追加");
        print_message_to_file(&message_path, "second\n").expect("追加");
        assert_eq!(
            std::fs::read_to_string(&message_path).expect("读取"),
            "first\nsecond\n"
        );

        print_status_to_file(&status_path, "旧的\n").expect("覆盖");
        print_status_to_file(&status_path, "新的\n").expect("覆盖");
        assert_eq!(
            std::fs::read_to_string(&status_path).expect("读取"),
            "新的\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
