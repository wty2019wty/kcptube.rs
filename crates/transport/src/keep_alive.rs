// SPDX-License-Identifier: GPL-3.0-only

//! 保活发送（对应 C++ 各 mode 中的 `loop_keep_alive`）。
//!
//! C++ 在 client / server / relay 里各写了一份「到点就发 `keep_alive`、
//! 记录发送时刻、推进下次时刻」的循环。这里把这段逻辑收成两种形态：
//!
//! - [`KeepAlive`]：自带下次时刻，适合把计时器放在 `select!` 循环的局部变量里；
//! - [`KeepAlive::tick_at`]：下次时刻由调用方保存（服务端把会话作为 `Arc`
//!   在全局维护任务里遍历，时刻存在 `DataSession` 的原子字段中）。
//!
//! 两者都只负责「发送 + 打点」，不负责判断对端是否还活着——超时判定见各 mode 的维护任务。

use crate::TransportError;
use crate::channel::KcpChannel;
use kcptube_protocol::packet::{ProtocolType, create_keep_alive_packet};

/// 保活发送计时器。
#[derive(Debug, Clone, Copy)]
pub struct KeepAlive {
    interval: i64,
    next_at: i64,
}

impl KeepAlive {
    /// `keep_alive <= 0` 表示关闭保活（下次时刻记为 `i64::MAX`）。
    pub fn new(keep_alive: i64, now: i64) -> Self {
        Self {
            interval: keep_alive,
            next_at: if keep_alive > 0 {
                now + keep_alive
            } else {
                i64::MAX
            },
        }
    }

    pub fn interval(&self) -> i64 {
        self.interval
    }

    pub fn next_at(&self) -> i64 {
        self.next_at
    }

    /// 把下次发送时刻设为 `at`（供调用方在校准时钟或迁移会话时使用）。
    pub fn set_next_at(&mut self, at: i64) {
        self.next_at = at;
    }

    /// 到点则发送一次 `keep_alive` 并记录发送时刻，返回是否真的发送。
    pub async fn tick(
        &mut self,
        channel: &KcpChannel,
        protocol: ProtocolType,
        now: i64,
    ) -> Result<bool, TransportError> {
        if self.interval <= 0 || now < self.next_at {
            return Ok(false);
        }
        channel.send(&create_keep_alive_packet(protocol)).await?;
        channel.mark_keep_alive_sent(now);
        self.next_at = now + self.interval;
        Ok(true)
    }

    /// 无状态版本：`next_at` 由调用方保存。
    ///
    /// 返回 `Some(下一次发送时刻)` 表示本次已发送；`None` 表示未到点或已关闭。
    /// 与 C++ 一致：调用方无论发送成功与否都应推进下次时刻，避免每个 tick 重试。
    pub async fn tick_at(
        channel: &KcpChannel,
        protocol: ProtocolType,
        interval: i64,
        next_at: i64,
        now: i64,
    ) -> Result<Option<i64>, TransportError> {
        if interval <= 0 || now < next_at {
            return Ok(None);
        }
        channel.send(&create_keep_alive_packet(protocol)).await?;
        channel.mark_keep_alive_sent(now);
        Ok(Some(now + interval))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_timer_never_fires() {
        let timer = KeepAlive::new(0, 1000);
        assert_eq!(timer.next_at(), i64::MAX);
        assert_eq!(timer.interval(), 0);
    }

    #[test]
    fn timer_schedules_next_from_now() {
        let mut timer = KeepAlive::new(10, 1000);
        assert_eq!(timer.next_at(), 1010);
        timer.set_next_at(1020);
        assert_eq!(timer.next_at(), 1020);
    }
}
