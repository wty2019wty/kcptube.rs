// SPDX-License-Identifier: GPL-3.0-only

//! 数据层与包层结构。
//!
//! 字节序约定（与 C++ 版一致，容易踩坑，务必注意）：
//! - `timestamp`：**小端**（C++ 用 `host_to_little_endian`）
//! - `sn`、`kcp_conv`、`connection_id`、`uid` 等：**大端**（C++ 用 `htonl` / `htonll`）
//! - `sub_sn`、端口号等单字节或位域：原样

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("数据长度不足")]
    TooShort,
    #[error("未知的 feature 值：{0}")]
    UnknownFeature(u8),
    #[error("未知的 protocol 值：{0}")]
    UnknownProtocol(u8),
    #[error("数据层为空")]
    EmptyDataLayer,
}

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

// ---- 包层编解码 ----

impl DataLayer {
    /// 编码「数据层 + 负载」。
    pub fn encode_packet(self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::SIZE + payload.len());
        out.push(self.to_byte());
        out.extend_from_slice(payload);
        out
    }

    /// 解码「数据层 + 负载」，返回头与负载切片。
    pub fn decode_packet(data: &[u8]) -> Result<(Self, &[u8]), ProtocolError> {
        let first = *data.first().ok_or(ProtocolError::EmptyDataLayer)?;
        let layer = Self::from_byte(first).ok_or_else(|| {
            let feature = first & 0x0F;
            let protocol = (first >> 4) & 0x0F;
            if Feature::from_u8(feature).is_none() {
                ProtocolError::UnknownFeature(feature)
            } else {
                ProtocolError::UnknownProtocol(protocol)
            }
        })?;
        Ok((layer, &data[Self::SIZE..]))
    }
}

impl PacketLayer {
    pub fn encode(timestamp: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_SIZE + data.len());
        out.extend_from_slice(&timestamp.to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    pub fn decode(data: &[u8]) -> Result<(Self, &[u8]), ProtocolError> {
        let timestamp = read_le_u32(data).ok_or(ProtocolError::TooShort)?;
        Ok((Self { timestamp }, &data[Self::HEADER_SIZE..]))
    }
}

impl PacketLayerData {
    pub fn encode(timestamp: u32, sn: u32, sub_sn: u8, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_SIZE + data.len());
        out.extend_from_slice(&timestamp.to_le_bytes());
        out.extend_from_slice(&sn.to_be_bytes());
        out.push(sub_sn);
        out.extend_from_slice(data);
        out
    }

    pub fn decode(data: &[u8]) -> Result<(Self, &[u8]), ProtocolError> {
        if data.len() < Self::HEADER_SIZE {
            return Err(ProtocolError::TooShort);
        }
        let timestamp = read_le_u32(data).ok_or(ProtocolError::TooShort)?;
        let sn = read_be_u32(&data[4..]).ok_or(ProtocolError::TooShort)?;
        let sub_sn = data[8];
        Ok((
            Self {
                timestamp,
                sn,
                sub_sn,
            },
            &data[Self::HEADER_SIZE..],
        ))
    }
}

impl PacketLayerFec {
    pub fn encode(timestamp: u32, sn: u32, sub_sn: u8, kcp_conv: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_SIZE + data.len());
        out.extend_from_slice(&timestamp.to_le_bytes());
        out.extend_from_slice(&sn.to_be_bytes());
        out.push(sub_sn);
        out.extend_from_slice(&kcp_conv.to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    pub fn decode(data: &[u8]) -> Result<(Self, &[u8]), ProtocolError> {
        if data.len() < Self::HEADER_SIZE {
            return Err(ProtocolError::TooShort);
        }
        let timestamp = read_le_u32(data).ok_or(ProtocolError::TooShort)?;
        let sn = read_be_u32(&data[4..]).ok_or(ProtocolError::TooShort)?;
        let sub_sn = data[8];
        let kcp_conv = read_be_u32(&data[9..]).ok_or(ProtocolError::TooShort)?;
        Ok((
            Self {
                timestamp,
                sn,
                sub_sn,
                kcp_conv,
            },
            &data[Self::HEADER_SIZE..],
        ))
    }
}

impl MuxDataWrapper {
    pub fn encode(connection_id: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_SIZE + data.len());
        out.extend_from_slice(&connection_id.to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    pub fn decode(data: &[u8]) -> Result<(Self, &[u8]), ProtocolError> {
        let connection_id = read_be_u32(data).ok_or(ProtocolError::TooShort)?;
        Ok((Self { connection_id }, &data[Self::HEADER_SIZE..]))
    }
}

// ---- settings_wrapper / pre_connect_custom_address ----

/// 对应 C++ `packet::settings_wrapper`（`#pragma pack(1)`）。
///
/// 固定的 26 字节头之后是**以 NUL 结尾**的 IP 字符串。
/// C++ 的 `sizeof` 为 27（`char[1]` 占一位），因此空 IP 时负载是 27 字节。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsWrapper {
    pub uid: u32,
    pub port_start: u16,
    pub port_end: u16,
    pub outbound_bandwidth: u64,
    pub inbound_bandwidth: u64,
    pub user_input_port: u16,
    pub user_input_ip: String,
}

impl SettingsWrapper {
    pub const HEADER_SIZE: usize = 26;
    /// C++ `sizeof(settings_wrapper)`
    pub const WIRE_SIZE: usize = Self::HEADER_SIZE + 1;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::WIRE_SIZE + self.user_input_ip.len());
        out.extend_from_slice(&self.uid.to_be_bytes());
        out.extend_from_slice(&self.port_start.to_be_bytes());
        out.extend_from_slice(&self.port_end.to_be_bytes());
        out.extend_from_slice(&self.outbound_bandwidth.to_be_bytes());
        out.extend_from_slice(&self.inbound_bandwidth.to_be_bytes());
        out.extend_from_slice(&self.user_input_port.to_be_bytes());
        out.extend_from_slice(self.user_input_ip.as_bytes());
        // 与 C++ 的 vector<u8>(sizeof(settings_wrapper)) 一致：末尾保留一个 0
        out.push(0);
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < Self::HEADER_SIZE {
            return Err(ProtocolError::TooShort);
        }
        let uid = read_be_u32(data).ok_or(ProtocolError::TooShort)?;
        let port_start = u16::from_be_bytes([data[4], data[5]]);
        let port_end = u16::from_be_bytes([data[6], data[7]]);
        let outbound_bandwidth = u64::from_be_bytes(
            data[8..16]
                .try_into()
                .map_err(|_| ProtocolError::TooShort)?,
        );
        let inbound_bandwidth = u64::from_be_bytes(
            data[16..24]
                .try_into()
                .map_err(|_| ProtocolError::TooShort)?,
        );
        let user_input_port = u16::from_be_bytes([data[24], data[25]]);
        let ip_bytes = &data[Self::HEADER_SIZE..];
        let end = ip_bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(ip_bytes.len());
        let user_input_ip = String::from_utf8_lossy(&ip_bytes[..end]).into_owned();

        Ok(Self {
            uid,
            port_start,
            port_end,
            outbound_bandwidth,
            inbound_bandwidth,
            user_input_port,
            user_input_ip,
        })
    }
}

/// 对应 C++ `packet::pre_connect_custom_address`
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreConnectCustomAddress {
    pub connection_id: u32,
    pub user_input_port: u16,
    pub user_input_ip: String,
}

impl PreConnectCustomAddress {
    pub const HEADER_SIZE: usize = 6;
    pub const WIRE_SIZE: usize = Self::HEADER_SIZE + 1;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::WIRE_SIZE + self.user_input_ip.len());
        out.extend_from_slice(&self.connection_id.to_be_bytes());
        out.extend_from_slice(&self.user_input_port.to_be_bytes());
        out.extend_from_slice(self.user_input_ip.as_bytes());
        out.push(0);
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < Self::HEADER_SIZE {
            return Err(ProtocolError::TooShort);
        }
        let connection_id = read_be_u32(data).ok_or(ProtocolError::TooShort)?;
        let user_input_port = u16::from_be_bytes([data[4], data[5]]);
        let ip_bytes = &data[Self::HEADER_SIZE..];
        let end = ip_bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(ip_bytes.len());
        Ok(Self {
            connection_id,
            user_input_port,
            user_input_ip: String::from_utf8_lossy(&ip_bytes[..end]).into_owned(),
        })
    }
}

// ---- 高层构造 / 解析（对应 C++ `packet` 命名空间里的自由函数） ----

/// C++ `packet::empty_data_size = sizeof(data_layer)`（2 字节）
pub const EMPTY_DATA_SIZE: usize = 2;

/// 对应 `create_inner_packet`：数据层头 + 负载
pub fn create_inner_packet(feature: Feature, protocol: ProtocolType, payload: &[u8]) -> Vec<u8> {
    DataLayer::new(feature, protocol).encode_packet(payload)
}

pub fn unpack_inner(data: &[u8]) -> Result<(Feature, ProtocolType, &[u8]), ProtocolError> {
    let (layer, payload) = DataLayer::decode_packet(data)?;
    Ok((layer.feature, layer.protocol, payload))
}

fn now_u32() -> u32 {
    crate::now_seconds() as u32
}

/// 对应 `create_packet`：包层头 + 已编码的数据层
pub fn create_packet(inner: &[u8]) -> Vec<u8> {
    create_packet_at(now_u32(), inner)
}

pub fn create_packet_at(timestamp: u32, inner: &[u8]) -> Vec<u8> {
    PacketLayer::encode(timestamp, inner)
}

/// 对应 `create_fec_data_packet`
pub fn create_fec_data_packet(inner: &[u8], sn: u32, sub_sn: u8) -> Vec<u8> {
    PacketLayerData::encode(now_u32(), sn, sub_sn, inner)
}

/// 对应 `create_fec_redundant_packet`
pub fn create_fec_redundant_packet(inner: &[u8], sn: u32, sub_sn: u8, kcp_conv: u32) -> Vec<u8> {
    PacketLayerFec::encode(now_u32(), sn, sub_sn, kcp_conv, inner)
}

/// 对应 `request_initialise_packet`（不带自定义地址）
pub fn request_initialise_packet(
    protocol: ProtocolType,
    outbound_bandwidth: u64,
    inbound_bandwidth: u64,
) -> Vec<u8> {
    request_initialise_packet_with_address(protocol, outbound_bandwidth, inbound_bandwidth, "", 0)
}

/// 对应带自定义地址的 `request_initialise_packet`
pub fn request_initialise_packet_with_address(
    protocol: ProtocolType,
    outbound_bandwidth: u64,
    inbound_bandwidth: u64,
    address: &str,
    port: u16,
) -> Vec<u8> {
    let settings = SettingsWrapper {
        uid: 0,
        port_start: 0,
        port_end: 0,
        outbound_bandwidth,
        inbound_bandwidth,
        user_input_port: port,
        user_input_ip: address.to_string(),
    };
    create_inner_packet(Feature::Initialise, protocol, &settings.encode())
}

/// 对应 `response_initialise_packet`
pub fn response_initialise_packet(protocol: ProtocolType, settings: &SettingsWrapper) -> Vec<u8> {
    create_inner_packet(Feature::Initialise, protocol, &settings.encode())
}

/// 对应 `create_test_connection_packet`（与 keep_alive 相同）
pub fn create_test_connection_packet() -> Vec<u8> {
    create_keep_alive_packet(ProtocolType::NotCare)
}

/// 对应 `inform_disconnect_packet`
pub fn inform_disconnect_packet(protocol: ProtocolType) -> Vec<u8> {
    create_inner_packet(Feature::Disconnect, protocol, &[0u8; EMPTY_DATA_SIZE])
}

/// 对应 `inform_error_packet`（负载末尾保留一个 0）
pub fn inform_error_packet(protocol: ProtocolType, message: &str) -> Vec<u8> {
    let mut payload = message.as_bytes().to_vec();
    payload.push(0);
    create_inner_packet(Feature::Failure, protocol, &payload)
}

/// 对应 `create_data_packet`
pub fn create_data_packet(protocol: ProtocolType, data: &[u8]) -> Vec<u8> {
    create_inner_packet(Feature::RawData, protocol, data)
}

/// 对应 `create_keep_alive_packet`
pub fn create_keep_alive_packet(protocol: ProtocolType) -> Vec<u8> {
    create_inner_packet(Feature::KeepAlive, protocol, &[0u8; EMPTY_DATA_SIZE])
}

/// 对应 `create_keep_alive_response_packet`
pub fn create_keep_alive_response_packet(protocol: ProtocolType) -> Vec<u8> {
    create_inner_packet(
        Feature::KeepAliveResponse,
        protocol,
        &[0u8; EMPTY_DATA_SIZE],
    )
}

/// 对应 `create_mux_data_packet`
pub fn create_mux_data_packet(protocol: ProtocolType, connection_id: u32, data: &[u8]) -> Vec<u8> {
    create_inner_packet(
        Feature::MuxTransfer,
        protocol,
        &MuxDataWrapper::encode(connection_id, data),
    )
}

/// 对应 `mux_tell_server_connect_address`
pub fn mux_tell_server_connect_address(
    protocol: ProtocolType,
    connection_id: u32,
    address: &str,
    port: u16,
) -> Vec<u8> {
    let pre = PreConnectCustomAddress {
        connection_id,
        user_input_port: port,
        user_input_ip: address.to_string(),
    };
    create_inner_packet(Feature::PreConnectCustomAddress, protocol, &pre.encode())
}

/// 对应 `inform_mux_cancel_packet`。
///
/// 注意 C++ 分配的是 `sizeof(data_layer) - 1 + sizeof(mux_data_wrapper)`，
/// 即 1（数据层头）+ 5（4 字节 id + 1 字节填充），所以线上会比 5 字节多一个 0。
pub fn inform_mux_cancel_packet(protocol: ProtocolType, connection_id: u32) -> Vec<u8> {
    let mut payload = Vec::with_capacity(MuxDataWrapper::HEADER_SIZE + 1);
    payload.extend_from_slice(&connection_id.to_be_bytes());
    payload.push(0);
    create_inner_packet(Feature::MuxCancel, protocol, &payload)
}

pub fn extract_mux_data(data: &[u8]) -> Result<(u32, &[u8]), ProtocolError> {
    let (wrapper, payload) = MuxDataWrapper::decode(data)?;
    Ok((wrapper.connection_id, payload))
}

pub fn extract_mux_pre_connect(data: &[u8]) -> Result<PreConnectCustomAddress, ProtocolError> {
    PreConnectCustomAddress::decode(data)
}

pub fn extract_mux_cancel(data: &[u8]) -> Result<u32, ProtocolError> {
    read_be_u32(data).ok_or(ProtocolError::TooShort)
}

/// 对应 `get_error_message_from_unpacked_data`
pub fn error_message(data: &[u8]) -> String {
    String::from_utf8_lossy(data).into_owned()
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

    #[test]
    fn packet_layer_encode_uses_little_endian_timestamp() {
        let out = PacketLayer::encode(0x0102_0304, &[0xAA]);
        assert_eq!(out, vec![0x04, 0x03, 0x02, 0x01, 0xAA]);
        let (header, payload) = PacketLayer::decode(&out).unwrap();
        assert_eq!(header.timestamp, 0x0102_0304);
        assert_eq!(payload, &[0xAA]);
    }

    #[test]
    fn fec_headers_are_big_endian() {
        let data = PacketLayerData::encode(0x0102_0304, 0x1122_3344, 7, &[9]);
        assert_eq!(&data[..4], &[0x04, 0x03, 0x02, 0x01]);
        assert_eq!(&data[4..8], &[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(data[8], 7);
        let (header, payload) = PacketLayerData::decode(&data).unwrap();
        assert_eq!(header.sn, 0x1122_3344);
        assert_eq!(payload, &[9]);

        let data = PacketLayerFec::encode(1, 2, 3, 0x0102_0304, &[4]);
        assert_eq!(&data[9..13], &[0x01, 0x02, 0x03, 0x04]);
        let (header, payload) = PacketLayerFec::decode(&data).unwrap();
        assert_eq!(header.kcp_conv, 0x0102_0304);
        assert_eq!(payload, &[4]);
    }

    #[test]
    fn inner_packets_match_cpp_layout() {
        assert_eq!(
            create_data_packet(ProtocolType::Tcp, b"hi"),
            vec![0x25, b'h', b'i']
        );
        assert_eq!(
            create_keep_alive_packet(ProtocolType::NotCare),
            vec![0x03, 0, 0]
        );
        assert_eq!(
            create_keep_alive_response_packet(ProtocolType::NotCare),
            vec![0x04, 0, 0]
        );
        assert_eq!(
            inform_disconnect_packet(ProtocolType::Udp),
            vec![0x32, 0, 0]
        );
        assert_eq!(
            inform_error_packet(ProtocolType::NotCare, "bad"),
            vec![0x01, b'b', b'a', b'd', 0]
        );

        let data_packet = create_data_packet(ProtocolType::Udp, b"x");
        let (feature, protocol, payload) = unpack_inner(&data_packet).unwrap();
        assert_eq!(feature, Feature::RawData);
        assert_eq!(protocol, ProtocolType::Udp);
        assert_eq!(payload, b"x");
    }

    #[test]
    fn mux_packets_match_cpp_layout() {
        let packet = create_mux_data_packet(ProtocolType::Tcp, 0x0102_0304, b"data");
        assert_eq!(&packet[..5], &[0x26, 0x01, 0x02, 0x03, 0x04]);
        let (_, protocol, payload) = unpack_inner(&packet).unwrap();
        assert_eq!(protocol, ProtocolType::Tcp);
        let (id, data) = extract_mux_data(payload).unwrap();
        assert_eq!(id, 0x0102_0304);
        assert_eq!(data, b"data");

        // C++ 的 mux_cancel 会多一个填充字节
        let cancel = inform_mux_cancel_packet(ProtocolType::Tcp, 5);
        assert_eq!(cancel, vec![0x27, 0, 0, 0, 5, 0]);
        let (_, _, payload) = unpack_inner(&cancel).unwrap();
        assert_eq!(extract_mux_cancel(payload).unwrap(), 5);

        let pre = mux_tell_server_connect_address(ProtocolType::Udp, 6, "1.2.3.4", 80);
        assert_eq!(pre.len(), 1 + 6 + "1.2.3.4".len() + 1);
        let (_, _, payload) = unpack_inner(&pre).unwrap();
        let parsed = extract_mux_pre_connect(payload).unwrap();
        assert_eq!(parsed.connection_id, 6);
        assert_eq!(parsed.user_input_port, 80);
        assert_eq!(parsed.user_input_ip, "1.2.3.4");
    }

    #[test]
    fn settings_wrapper_matches_cpp_size() {
        let settings = SettingsWrapper {
            uid: 0x0102_0304,
            port_start: 3000,
            port_end: 3010,
            outbound_bandwidth: 1024,
            inbound_bandwidth: 2048,
            user_input_port: 5099,
            user_input_ip: String::new(),
        };
        let encoded = settings.encode();
        // sizeof(settings_wrapper) == 27
        assert_eq!(encoded.len(), 27);
        assert_eq!(&encoded[..4], &[1, 2, 3, 4]);
        assert_eq!(&encoded[4..6], &3000u16.to_be_bytes());
        assert_eq!(encoded[26], 0);
        assert_eq!(SettingsWrapper::decode(&encoded).unwrap(), settings);

        let with_ip =
            request_initialise_packet_with_address(ProtocolType::Tcp, 1, 2, "example.com", 443);
        assert_eq!(with_ip.len(), 1 + 27 + "example.com".len());
        let (feature, protocol, payload) = unpack_inner(&with_ip).unwrap();
        assert_eq!(feature, Feature::Initialise);
        assert_eq!(protocol, ProtocolType::Tcp);
        let settings = SettingsWrapper::decode(payload).unwrap();
        assert_eq!(settings.user_input_ip, "example.com");
        assert_eq!(settings.user_input_port, 443);
        assert_eq!(settings.outbound_bandwidth, 1);
        assert_eq!(settings.inbound_bandwidth, 2);
    }

    #[test]
    fn truncated_inputs_are_rejected() {
        assert_eq!(
            PacketLayer::decode(&[1, 2, 3]),
            Err(ProtocolError::TooShort)
        );
        assert_eq!(
            PacketLayerData::decode(&[0; 8]),
            Err(ProtocolError::TooShort)
        );
        assert_eq!(
            DataLayer::decode_packet(&[]),
            Err(ProtocolError::EmptyDataLayer)
        );
        assert_eq!(
            DataLayer::decode_packet(&[0x0F]),
            Err(ProtocolError::UnknownFeature(0x0F))
        );
    }
}
