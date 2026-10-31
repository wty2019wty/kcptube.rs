// SPDX-License-Identifier: GPL-3.0-only

//! 数据层与包层结构。
//!
//! 字节序约定（与 C++ 版一致，容易踩坑，务必注意）：
//! - `timestamp`：**小端**（C++ 用 `host_to_little_endian`）
//! - `sn`、`kcp_conv`、`connection_id`、`uid` 等：**大端**（C++ 用 `htonl` / `htonll`）
//! - `sub_sn`、端口号等单字节或位域：原样

/// 数据层「特性」字段（低 4 位）。
///
/// 对应 C++ `enum class feature : uint8_t`。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// 初始化（握手请求/响应）
    Initialise = 0,
    /// 失败
    Failure = 1,
    /// 断开
    Disconnect = 2,
    /// 保活（同时也是连通性测试包）
    KeepAlive = 3,
    /// 保活响应
    KeepAliveResponse = 4,
    /// 原始数据
    RawData = 5,
    /// mux 转发
    MuxTransfer = 6,
    /// mux 取消
    MuxCancel = 7,
    /// 预连接自定义地址（服务端专用）
    PreConnectCustomAddress = 8,
}

impl Feature {
    /// C++ 中 `test_connection` 与 `keep_alive` 共用同一枚举值（=3）。
    pub const TEST_CONNECTION: Feature = Feature::KeepAlive;

    pub const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Feature::Initialise,
            1 => Feature::Failure,
            2 => Feature::Disconnect,
            3 => Feature::KeepAlive,
            4 => Feature::KeepAliveResponse,
            5 => Feature::RawData,
            6 => Feature::MuxTransfer,
            7 => Feature::MuxCancel,
            8 => Feature::PreConnectCustomAddress,
            _ => return None,
        })
    }
}

/// 数据层「协议类型」字段（高 4 位）。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolType {
    NotCare = 0,
    Mux = 1,
    Tcp = 2,
    Udp = 3,
}

impl ProtocolType {
    pub const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => ProtocolType::NotCare,
            1 => ProtocolType::Mux,
            2 => ProtocolType::Tcp,
            3 => ProtocolType::Udp,
            _ => return None,
        })
    }
}

/// `data_layer`：1 字节头（feature 占低 4 位，protocol 占高 4 位）+ 负载。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataLayer {
    pub feature: Feature,
    pub protocol: ProtocolType,
}

impl DataLayer {
    pub const SIZE: usize = 1;

    pub const fn new(feature: Feature, protocol: ProtocolType) -> Self {
        Self { feature, protocol }
    }

    /// 编码为 1 字节（对应 C++ 位域布局：低 4 位 feature，高 4 位 protocol）。
    pub const fn to_byte(self) -> u8 {
        (self.feature as u8 & 0x0F) | ((self.protocol as u8 & 0x0F) << 4)
    }

    pub fn from_byte(byte: u8) -> Option<Self> {
        let feature = Feature::from_u8(byte & 0x0F)?;
        let protocol = ProtocolType::from_u8((byte >> 4) & 0x0F)?;
        Some(Self { feature, protocol })
    }
}

// ---- 包层头部 ----

/// `packet_layer`：timestamp(u32 LE) + data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketLayer {
    pub timestamp: u32,
}

/// `packet_layer_data`：timestamp(u32 LE) + sn(u32 BE) + sub_sn(u8) + data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketLayerData {
    pub timestamp: u32,
    pub sn: u32,
    pub sub_sn: u8,
}

/// `packet_layer_fec`：timestamp(u32 LE) + sn(u32 BE) + sub_sn(u8) + kcp_conv(u32 BE) + data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketLayerFec {
    pub timestamp: u32,
    pub sn: u32,
    pub sub_sn: u8,
    pub kcp_conv: u32,
}

/// `mux_data_wrapper`：connection_id(u32 BE) + data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MuxDataWrapper {
    pub connection_id: u32,
}

impl PacketLayer {
    pub const HEADER_SIZE: usize = crate::constant::PACKET_LAYER_HEADER;
}

impl PacketLayerData {
    pub const HEADER_SIZE: usize = crate::constant::PACKET_LAYER_DATA_HEADER;
}

impl PacketLayerFec {
    pub const HEADER_SIZE: usize = crate::constant::PACKET_LAYER_FEC_HEADER;
}

impl MuxDataWrapper {
    pub const HEADER_SIZE: usize = crate::constant::MUX_DATA_WRAPPER_HEADER;
}

// ---- 小工具：按 C++ 的字节序约定读写 ----

/// 读小端 u32（timestamp 用）
pub fn read_le_u32(data: &[u8]) -> Option<u32> {
    data.get(..4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// 读大端 u32（sn / kcp_conv / connection_id 用）
pub fn read_be_u32(data: &[u8]) -> Option<u32> {
    data.get(..4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// 写小端 u32
pub fn write_le_u32(out: &mut [u8], value: u32) {
    if let Some(dst) = out.get_mut(..4) {
        dst.copy_from_slice(&value.to_le_bytes());
    }
}

/// 写大端 u32
pub fn write_be_u32(out: &mut [u8], value: u32) {
    if let Some(dst) = out.get_mut(..4) {
        dst.copy_from_slice(&value.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_layer_bit_layout_matches_cpp() {
        // C++ 位域：feature 在低 4 位，protocol 在高 4 位
        let dl = DataLayer::new(Feature::RawData, ProtocolType::Tcp);
        assert_eq!(dl.to_byte(), 0x25); // 0b0010_0101
        assert_eq!(DataLayer::from_byte(0x25), Some(dl));

        let dl = DataLayer::new(Feature::KeepAlive, ProtocolType::NotCare);
        assert_eq!(dl.to_byte(), 0x03);
    }

    #[test]
    fn feature_values_match_cpp() {
        assert_eq!(Feature::Initialise as u8, 0);
        assert_eq!(Feature::Failure as u8, 1);
        assert_eq!(Feature::Disconnect as u8, 2);
        assert_eq!(Feature::KeepAlive as u8, 3);
        assert_eq!(Feature::TEST_CONNECTION as u8, 3);
        assert_eq!(Feature::KeepAliveResponse as u8, 4);
        assert_eq!(Feature::RawData as u8, 5);
        assert_eq!(Feature::MuxTransfer as u8, 6);
        assert_eq!(Feature::MuxCancel as u8, 7);
        assert_eq!(Feature::PreConnectCustomAddress as u8, 8);
    }

    #[test]
    fn header_sizes_match_cpp_packed_structs() {
        use crate::constant::*;
        assert_eq!(PacketLayer::HEADER_SIZE, 4);
        assert_eq!(PacketLayerData::HEADER_SIZE, 9);
        assert_eq!(PacketLayerFec::HEADER_SIZE, 13);
        assert_eq!(MuxDataWrapper::HEADER_SIZE, 4);
        assert_eq!(PACKET_LAYER_FEC_HEADER, PACKET_LAYER_DATA_HEADER + 4);
    }

    #[test]
    fn endian_helpers() {
        let mut buf = [0u8; 4];
        write_le_u32(&mut buf, 0x0102_0304);
        assert_eq!(buf, [0x04, 0x03, 0x02, 0x01]);
        assert_eq!(read_le_u32(&buf), Some(0x0102_0304));

        write_be_u32(&mut buf, 0x0102_0304);
        assert_eq!(buf, [0x01, 0x02, 0x03, 0x04]);
        assert_eq!(read_be_u32(&buf), Some(0x0102_0304));
    }
}
