//! KCP 传输层。
//!
//! ## 为什么不用现成 crate
//!
//! C++ 版使用的 `legacy-cpp/src/3rd_party/ikcp.cpp` 是 skywind3000/ikcp 的
//! cnbatch 改版，额外具备：
//!
//! - 带宽限制 / 平均速率统计（`SetBandwidth`）
//! - 窗口自适应（`ResetWindowValues`）
//! - `resendts_buf` / `fastack_buf` 索引，降低扫描开销
//! - `WaitQueue*` 系列背压接口
//!
//! 线格式仍是标准 ikcp（conv/cmd/frg/wnd/ts/sn/una/len 大端），因此
//! **可以**用现成 crate；但为了与 C++ 版的拥塞/重传行为保持一致，
//! 建议逐行移植这 1100 行（纯逻辑、无平台依赖）。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum KcpError {
    #[error("KCP 未初始化")]
    NotInitialised,
    #[error("发送缓冲区已满")]
    SendQueueFull,
    #[error("功能尚未实现：{0}")]
    NotImplemented(&'static str),
}

/// KCP 参数（对应 C++ `user_settings.kcp_*` 与命名预设 regular1-5 / fast1-6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KcpConfig {
    pub conv: u32,
    pub mtu: usize,
    /// 是否启用 nodelay
    pub nodelay: bool,
    /// 内部刷新间隔（毫秒）
    pub interval: u32,
    /// 快速重传阈值（0 = 关闭）
    pub resend: u32,
    /// 是否关闭拥塞控制
    pub no_congestion_control: bool,
    pub send_window: u32,
    pub receive_window: u32,
    /// 流模式：把小包合并发送
    pub stream_mode: bool,
}

impl Default for KcpConfig {
    fn default() -> Self {
        Self {
            conv: 0,
            mtu: crate::MTU_WITH_FEC_DEFAULT,
            nodelay: true,
            interval: 10,
            resend: 2,
            no_congestion_control: true,
            send_window: 1024,
            receive_window: 1024,
            stream_mode: false,
        }
    }
}

/// 默认 KCP MTU（对应 C++ 的 `kcp_mtu_with_fec`）
pub const MTU_WITH_FEC_DEFAULT: usize = 1405;

/// KCP 输出回调：底层发送一个 UDP 负载
pub type OutputFn = Box<dyn Fn(&[u8]) -> Result<(), KcpError> + Send + Sync>;

/// KCP 会话
pub struct Kcp {
    config: KcpConfig,
    output: Option<OutputFn>,
}

impl Kcp {
    pub fn new(config: KcpConfig) -> Self {
        Self {
            config,
            output: None,
        }
    }

    pub fn conv(&self) -> u32 {
        self.config.conv
    }

    pub fn set_output(&mut self, output: OutputFn) {
        self.output = Some(output);
    }

    /// 上层发送数据（对应 `ikcp_send`）
    pub fn send(&mut self, _data: &[u8]) -> Result<(), KcpError> {
        Err(KcpError::NotImplemented("KCP send"))
    }

    /// 收到底层数据包（对应 `ikcp_input`）
    pub fn input(&mut self, _data: &[u8]) -> Result<(), KcpError> {
        Err(KcpError::NotImplemented("KCP input"))
    }

    /// 上层接收数据（对应 `ikcp_recv`）
    pub fn receive(&mut self, _buf: &mut [u8]) -> Result<usize, KcpError> {
        Err(KcpError::NotImplemented("KCP receive"))
    }

    /// 定时驱动（对应 `ikcp_update`）
    pub fn update(&mut self, _now_ms: u32) {}

    /// 下次需要驱动的时刻（对应 `ikcp_check`）
    pub fn check(&self, now_ms: u32) -> u32 {
        now_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_matches_cpp() {
        let config = KcpConfig::default();
        assert_eq!(config.send_window, 1024);
        assert_eq!(config.receive_window, 1024);
    }

    #[test]
    fn conv_roundtrip() {
        let kcp = Kcp::new(KcpConfig {
            conv: 0x1234_5678,
            ..Default::default()
        });
        assert_eq!(kcp.conv(), 0x1234_5678);
    }
}
