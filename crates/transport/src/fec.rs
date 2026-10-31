// SPDX-License-Identifier: GPL-3.0-only

//! FEC 收发流水线（对应 C++ `kcp_mappings::fec_control_data` 的发送/接收部分）。
//!
//! 发送侧（C++ `fec_maker`）：
//! 1. 每个 KCP 段前加 `packet_layer_data`（sn + sub_sn）；
//! 2. 缓存原始段，攒够 `k` 个后用 `fec_container` 对齐拼接并编码；
//! 3. 只把**冗余 share**（索引 >= k）配上 `packet_layer_fec`（含 `kcp_conv`）发出；
//! 4. `sub_sn` 从 0 递增到 `n-1`，然后归零、`sn` 加一。
//!
//! 接收侧（C++ `fec_find_missings`）：按 `sn` 缓存到 `sub_sn`，凑够 `k` 个就解码，
//! 只把**原本缺失**的原始段交给 KCP；`sn` 落后超过 `FEC_WAITS` 的代次被清理。

use kcptube_fec::{FecConfig, FecError, Zfec, pack_containers, unpack_container};
use kcptube_protocol::packet::{create_fec_data_packet, create_fec_redundant_packet};
use std::collections::BTreeMap;

/// 冗余包可跨越的代数（对应 C++ `gbv_fec_waits`）
pub const FEC_WAITS: u32 = 3;

/// 发送侧状态。
pub struct FecSender {
    zfec: Zfec,
    original: usize,
    conv: u32,
    sn: u32,
    sub_sn: u8,
    cache: Vec<Vec<u8>>,
}

impl FecSender {
    /// FEC 未启用（k 或 r 为 0）时返回 `None`。
    pub fn new(original: u8, redundant: u8, conv: u32) -> Result<Option<Self>, FecError> {
        let config = FecConfig {
            original: original as usize,
            redundant: redundant as usize,
        };
        if !config.is_enabled() {
            return Ok(None);
        }
        Ok(Some(Self {
            zfec: Zfec::new(config)?,
            original: config.original,
            conv,
            sn: 0,
            sub_sn: 0,
            cache: Vec::new(),
        }))
    }

    /// 处理一个 KCP 段，返回 `(原始包明文, 本轮产生的冗余包明文)`。
    pub fn wrap(&mut self, segment: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        // 握手通道（conv == 0）不做冗余，只加 FEC 头并保持 sub_sn 为 0
        if self.conv == 0 {
            self.sub_sn = 0;
            return (create_fec_data_packet(segment, self.sn, 0), Vec::new());
        }

        let original = create_fec_data_packet(segment, self.sn, self.sub_sn);
        self.sub_sn = self.sub_sn.wrapping_add(1);
        self.cache.push(segment.to_vec());

        let mut redundant = Vec::new();
        if self.cache.len() == self.original {
            let refs: Vec<&[u8]> = self.cache.iter().map(Vec::as_slice).collect();
            let (container, _align) = pack_containers(&refs);

            let sn = self.sn;
            let conv = self.conv;
            let original_count = self.original;
            let mut sub_sn = self.sub_sn;
            let mut shares = Vec::new();
            if self
                .zfec
                .encode(&container, |index, share| {
                    if index >= original_count {
                        shares.push(create_fec_redundant_packet(share, sn, sub_sn, conv));
                        sub_sn = sub_sn.wrapping_add(1);
                    }
                })
                .is_ok()
            {
                redundant = shares;
            }

            self.sub_sn = 0;
            self.sn = self.sn.wrapping_add(1);
            self.cache.clear();
        }

        (original, redundant)
    }
}

/// 接收侧状态。
pub struct FecReceiver {
    zfec: Zfec,
    original: usize,
    total: usize,
    /// sn → sub_sn → 段数据（原始段或冗余 share）
    cache: BTreeMap<u32, BTreeMap<u8, Vec<u8>>>,
    /// 已成功恢复的代次
    restored: BTreeMap<u32, ()>,
    latest_sn: u32,
}

impl FecReceiver {
    /// FEC 未启用时返回 `None`。
    pub fn new(original: u8, redundant: u8) -> Result<Option<Self>, FecError> {
        let config = FecConfig {
            original: original as usize,
            redundant: redundant as usize,
        };
        if !config.is_enabled() {
            return Ok(None);
        }
        Ok(Some(Self {
            zfec: Zfec::new(config)?,
            original: config.original,
            total: config.total(),
            cache: BTreeMap::new(),
            restored: BTreeMap::new(),
            latest_sn: 0,
        }))
    }

    /// 收到原始包（`sub_sn < k`）：返回本轮新恢复出的段。
    pub fn push_original(&mut self, sn: u32, sub_sn: u8, segment: &[u8]) -> Vec<Vec<u8>> {
        self.push(sn, sub_sn, segment)
    }

    /// 收到冗余包（`sub_sn >= k`）：返回本轮新恢复出的段。
    pub fn push_redundant(&mut self, sn: u32, sub_sn: u8, share: &[u8]) -> Vec<Vec<u8>> {
        self.push(sn, sub_sn, share)
    }

    fn push(&mut self, sn: u32, sub_sn: u8, data: &[u8]) -> Vec<Vec<u8>> {
        self.latest_sn = sn;
        self.cache
            .entry(sn)
            .or_default()
            .insert(sub_sn, data.to_vec());

        let recovered = self.try_recover(sn);
        self.cleanup();
        recovered
    }

    /// 凑够 `k` 个 share 就解码，只返回原本缺失的段。
    fn try_recover(&mut self, sn: u32) -> Vec<Vec<u8>> {
        if self.restored.contains_key(&sn) {
            return Vec::new();
        }

        let Some(entry) = self.cache.get(&sn) else {
            return Vec::new();
        };
        if entry.len() < self.original {
            return Vec::new();
        }

        // 对应 C++ `compact_into_container(cache, k)`：
        // 原始段补上 2 字节长度前缀，冗余 share 直接使用；统一补齐到同一长度。
        // 只要存在缺失的原始段，就必然至少有一个冗余 share 在缓存里，
        // 而对齐长度由 share 长度决定，因此这里算出的值一定与发送端一致。
        let mut align = 0usize;
        for (index, data) in entry {
            let size = if (*index as usize) < self.original {
                data.len() + 2
            } else {
                data.len()
            };
            align = align.max(size);
        }
        if align == 0 {
            return Vec::new();
        }

        let mut shares: Vec<Option<Vec<u8>>> = vec![None; self.total];
        for (index, data) in entry {
            let mut buffer = vec![0u8; align];
            if (*index as usize) < self.original {
                buffer[..2].copy_from_slice(&(data.len() as u16).to_be_bytes());
                buffer[2..2 + data.len()].copy_from_slice(data);
            } else if data.len() <= align {
                buffer[..data.len()].copy_from_slice(data);
            }
            if let Some(slot) = shares.get_mut(*index as usize) {
                *slot = Some(buffer);
            }
        }

        let Ok(container) = self.zfec.decode(&shares) else {
            return Vec::new();
        };
        let align = container.len() / self.original;
        if align == 0 {
            return Vec::new();
        }

        let mut recovered = Vec::new();
        for index in 0..self.original {
            if entry.contains_key(&(index as u8)) {
                continue;
            }
            let start = index * align;
            if let Ok(segment) = unpack_container(&container[start..start + align]) {
                recovered.push(segment.to_vec());
            }
        }

        self.restored.insert(sn, ());
        recovered
    }

    /// 清理落后超过 `FEC_WAITS` 代的记录（对应 C++ `fec_find_missings` 的擦除分支）。
    fn cleanup(&mut self) {
        let latest = self.latest_sn;
        self.cache
            .retain(|sn, _| latest.wrapping_sub(*sn) <= FEC_WAITS);
        self.restored
            .retain(|sn, _| latest.wrapping_sub(*sn) <= FEC_WAITS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sender_and_receiver(k: u8, r: u8) -> (FecSender, FecReceiver) {
        (
            FecSender::new(k, r, 42).unwrap().unwrap(),
            FecReceiver::new(k, r).unwrap().unwrap(),
        )
    }

    #[test]
    fn disabled_config_returns_none() {
        assert!(FecSender::new(0, 0, 1).unwrap().is_none());
        assert!(FecSender::new(4, 0, 1).unwrap().is_none());
        assert!(FecReceiver::new(0, 2).unwrap().is_none());
    }

    #[test]
    fn sender_emits_redundant_after_k_segments() {
        let (mut sender, _receiver) = sender_and_receiver(4, 2);
        for index in 0..3 {
            let (original, redundant) = sender.wrap(format!("seg-{index}").as_bytes());
            assert!(!original.is_empty());
            assert!(redundant.is_empty(), "未攒够 k 个不应产生冗余");
        }
        let (_original, redundant) = sender.wrap(b"seg-3");
        assert_eq!(redundant.len(), 2, "k=4,r=2 应产生 2 个冗余包");
    }

    #[test]
    fn receiver_recovers_one_missing_segment() {
        let (mut sender, mut receiver) = sender_and_receiver(4, 2);

        let segments: Vec<Vec<u8>> = (0..4)
            .map(|index| format!("payload-{index}").into_bytes())
            .collect();
        let mut originals = Vec::new();
        let mut redundant = Vec::new();
        for segment in &segments {
            let (original, extra) = sender.wrap(segment);
            originals.push(original);
            redundant.extend(extra);
        }
        assert_eq!(redundant.len(), 2);

        // 丢掉第 2 个原始包
        for (index, frame) in originals.iter().enumerate() {
            if index == 2 {
                continue;
            }
            let (header, payload) =
                kcptube_protocol::packet::PacketLayerData::decode(frame).expect("原始包头应可解析");
            assert!(
                receiver
                    .push_original(header.sn, header.sub_sn, payload)
                    .is_empty()
            );
        }

        // 冗余包到达后应恢复出第 2 段
        let mut recovered = Vec::new();
        for frame in &redundant {
            let (header, payload) =
                kcptube_protocol::packet::PacketLayerFec::decode(frame).expect("冗余包头应可解析");
            recovered.extend(receiver.push_redundant(header.sn, header.sub_sn, payload));
        }
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0], segments[2]);
    }

    #[test]
    fn receiver_needs_k_shares() {
        let (mut sender, mut receiver) = sender_and_receiver(3, 1);
        let mut originals = Vec::new();
        let mut redundant = Vec::new();
        for index in 0..3u8 {
            let (original, extra) = sender.wrap(&[index; 10]);
            originals.push(original);
            redundant.extend(extra);
        }

        // 只给 1 个原始包 + 1 个冗余，共 2 个 share，少于 k=3，不能恢复
        let (header, payload) =
            kcptube_protocol::packet::PacketLayerData::decode(&originals[0]).unwrap();
        assert!(
            receiver
                .push_original(header.sn, header.sub_sn, payload)
                .is_empty()
        );
        let (header, payload) =
            kcptube_protocol::packet::PacketLayerFec::decode(&redundant[0]).unwrap();
        assert!(
            receiver
                .push_redundant(header.sn, header.sub_sn, payload)
                .is_empty()
        );
    }

    #[test]
    fn old_generations_are_cleaned_up() {
        let (mut sender, mut receiver) = sender_and_receiver(2, 1);
        for index in 0..20u8 {
            let (original, _) = sender.wrap(&[index; 8]);
            let (header, payload) =
                kcptube_protocol::packet::PacketLayerData::decode(&original).unwrap();
            receiver.push_original(header.sn, header.sub_sn, payload);
        }
        assert!(
            receiver.cache.len() <= (FEC_WAITS + 1) as usize,
            "应只保留最近 {} 代，实际 {}",
            FEC_WAITS + 1,
            receiver.cache.len()
        );
    }
}
