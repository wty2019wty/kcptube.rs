// SPDX-License-Identifier: GPL-3.0-only

use super::*;
use std::sync::{Arc, Mutex};

fn make_kcp() -> (Kcp, Arc<Mutex<Vec<Vec<u8>>>>) {
    let packets = Arc::new(Mutex::new(Vec::new()));
    let sink = packets.clone();
    let mut kcp = Kcp::new(KcpConfig {
        conv: 0x0102_0304,
        mtu: 1400,
        nodelay: 1,
        interval: 10,
        resend: 2,
        no_congestion_control: true,
        send_window: 32,
        receive_window: 128,
        stream_mode: false,
    });
    kcp.set_output(Box::new(move |data: &[u8]| {
        sink.lock().unwrap().push(data.to_vec());
        Ok(())
    }));
    (kcp, packets)
}

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
    assert_eq!(Kcp::peek_conv(&[0x78, 0x56, 0x34, 0x12]), Some(0x1234_5678));
}

#[test]
fn update_without_data_emits_nothing() {
    let (mut kcp, packets) = make_kcp();
    assert_eq!(kcp.update(1000), 0);
    assert!(packets.lock().unwrap().is_empty());
}

#[test]
fn send_then_update_emits_one_segment() {
    let (mut kcp, packets) = make_kcp();
    kcp.update(1000);
    kcp.send(b"hello").unwrap();
    assert_eq!(kcp.waiting_for_send(), 1);
    kcp.update(1010);

    let packets = packets.lock().unwrap();
    assert_eq!(packets.len(), 1);
    let packet = &packets[0];
    assert_eq!(packet.len(), OVERHEAD as usize + 5);
    assert_eq!(&packet[..4], &0x0102_0304u32.to_le_bytes());
    assert_eq!(packet[4], CMD_PUSH);
    assert_eq!(packet[5], 0); // frg
    assert_eq!(packet[8..12], 1010u32.to_le_bytes()); // ts
    assert_eq!(packet[12..16], 0u32.to_le_bytes()); // sn
    assert_eq!(packet[16..20], 0u32.to_le_bytes()); // una
    assert_eq!(packet[20..24], 5u32.to_le_bytes()); // len
    assert_eq!(&packet[24..], b"hello");
    assert_eq!(kcp.waiting_for_send(), 1); // 已进入 snd_buf
}

#[test]
fn fragmentation_sets_frg_order() {
    let (mut kcp, packets) = make_kcp();
    kcp.set_mtu(100).unwrap();
    kcp.update(1000);
    let data = vec![0xAB; 200];
    kcp.send(&data).unwrap();
    kcp.update(1010);

    let packets = packets.lock().unwrap();
    assert!(packets.len() >= 2);
    let frg: Vec<u8> = packets.iter().map(|packet| packet[5]).collect();
    // mss = 76，200 字节拆成 3 段：frg = 2, 1, 0
    assert_eq!(frg, vec![2, 1, 0]);
}

#[test]
fn input_ack_removes_from_snd_buf() {
    let (mut kcp, packets) = make_kcp();
    kcp.update(1000);
    kcp.send(b"hello").unwrap();
    kcp.update(1010);
    let sent_ts = {
        let packets = packets.lock().unwrap();
        u32::from_le_bytes([packets[0][8], packets[0][9], packets[0][10], packets[0][11]])
    };

    let mut ack = [0u8; OVERHEAD as usize];
    ack[..4].copy_from_slice(&0x0102_0304u32.to_le_bytes());
    ack[4] = CMD_ACK;
    ack[8..12].copy_from_slice(&sent_ts.to_le_bytes());
    ack[12..16].copy_from_slice(&0u32.to_le_bytes());
    // 让 current 前进 20ms，使 RTT 采样非 0
    kcp.update(1030);
    kcp.input(&ack).unwrap();
    assert_eq!(kcp.waiting_for_send(), 0);
    assert!(kcp.rx_srtt() > 0);
}

#[test]
fn receive_reassembles_fragments() {
    // 直接用本端编码的段喂给另一个 KCP
    let (mut sender, packets) = make_kcp();
    sender.set_mtu(100).unwrap();
    sender.update(1000);
    let data = vec![0x5A; 200];
    sender.send(&data).unwrap();
    sender.update(1010);

    let (mut receiver, _) = make_kcp();
    receiver.update(1000);
    for packet in packets.lock().unwrap().iter() {
        receiver.input(packet).unwrap();
    }
    let mut buffer = vec![0u8; 300];
    let size = receiver.receive(&mut buffer).unwrap();
    assert_eq!(size, data.len());
    assert_eq!(&buffer[..size], &data[..]);
}

#[test]
fn rto_retransmits_without_ack() {
    let (mut kcp, packets) = make_kcp();
    kcp.update(1000);
    kcp.send(b"hello").unwrap();
    kcp.update(1010);
    assert_eq!(packets.lock().unwrap().len(), 1);

    // 初始 rx_rto = 200，nodelay=1 -> rtomin = 0，resendts = 1210
    kcp.update(1210);
    assert_eq!(packets.lock().unwrap().len(), 2);
    assert!(!kcp.is_dead());
}

#[test]
fn window_size_has_rcv_floor() {
    let mut kcp = Kcp::new(KcpConfig::default());
    kcp.set_window_size(0, 8);
    assert_eq!(kcp.window_sizes(), (1024, 128));
}

#[test]
fn bandwidth_reset_uses_min_rtt() {
    let mut kcp = Kcp::new(KcpConfig::default());
    kcp.set_bandwidth(1_000_000, 2_000_000);
    kcp.reset_window_values(100);
    let (snd, rcv) = kcp.window_sizes();
    assert!(snd >= 32);
    assert!(rcv >= 32);
}

/// 与 C++ 版同一调用序列的输出逐字节对撞。
///
/// C++ 探针：`KCP::KCP` + `SetMTU(1400)` + `SetWindowSize(32,128)` +
/// `NoDelay(1,10,2,1)`，依次 Update(1000) / Send("hello") / Update(1010) /
/// Input(ACK sn=0) / Update(1030) / Update(1040) / SetMTU(100) /
/// Send(200 字节) / Update(1050)。期望值由该探针打印。
#[test]
fn output_matches_cpp_transcript() {
    let (mut kcp, packets) = make_kcp();
    kcp.update(1000);
    kcp.send(b"hello").unwrap();
    kcp.update(1010);

    let mut ack = [0u8; OVERHEAD as usize];
    ack[..4].copy_from_slice(&0x0102_0304u32.to_le_bytes());
    ack[4] = CMD_ACK;
    ack[6..8].copy_from_slice(&128u16.to_le_bytes());
    ack[8..12].copy_from_slice(&1010u32.to_le_bytes());
    kcp.input(&ack).unwrap();
    kcp.update(1030);
    kcp.update(1040);

    kcp.set_mtu(100).unwrap();
    let big: Vec<u8> = (0..200u16).map(|value| value as u8).collect();
    kcp.send(&big).unwrap();
    kcp.update(1050);

    let actual: Vec<String> = packets
        .lock()
        .unwrap()
        .iter()
        .map(hex::encode_upper)
        .collect();
    let expected = vec![
        "0403020151008000F203000000000000000000000500000068656C6C6F",
        concat!(
            "04030201510280001A04000001000000000000004C000000",
            "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F",
            "202122232425262728292A2B2C2D2E2F303132333435363738393A3B3C3D3E3F",
            "404142434445464748494A4B"
        ),
        concat!(
            "04030201510180001A04000002000000000000004C000000",
            "4C4D4E4F505152535455565758595A5B5C5D5E5F606162636465666768696A6B",
            "6C6D6E6F707172737475767778797A7B7C7D7E7F808182838485868788898A8B8C",
            "8D8E8F9091929394959697"
        ),
        concat!(
            "04030201510080001A040000030000000000000030000000",
            "98999A9B9C9D9E9FA0A1A2A3A4A5A6A7A8A9AAABACADAEAF",
            "B0B1B2B3B4B5B6B7B8B9BABBBCBDBEBFC0C1C2C3C4C5C6C7"
        ),
    ];
    assert_eq!(actual, expected);
}

/// 10% 丢包的进程内回环：验证重传、ACK、窗口推进能把数据完整送达。
#[test]
fn lossy_loopback_delivers_all_data() {
    let (mut sender, sender_out) = make_kcp();
    let (mut receiver, receiver_out) = make_kcp();

    let messages: Vec<Vec<u8>> = (0..40u8)
        .map(|index| vec![index; 100 + index as usize * 7])
        .collect();
    let expected: Vec<u8> = messages.concat();
    for message in &messages {
        sender.send(message).unwrap();
    }

    let mut now = 1000u32;
    let mut packet_sequence = 0u64;
    let mut received = Vec::new();
    let mut buffer = vec![0u8; 4096];

    while received.len() < expected.len() && now < 60_000 {
        let packets: Vec<Vec<u8>> = sender_out.lock().unwrap().drain(..).collect();
        for packet in packets {
            packet_sequence += 1;
            if packet_sequence % 9 != 0 {
                receiver.input(&packet).unwrap();
            }
        }

        let packets: Vec<Vec<u8>> = receiver_out.lock().unwrap().drain(..).collect();
        for packet in packets {
            packet_sequence += 1;
            if packet_sequence % 9 != 0 {
                sender.input(&packet).unwrap();
            }
        }

        while let Ok(size) = receiver.receive(&mut buffer) {
            received.extend_from_slice(&buffer[..size]);
        }

        sender.update(now);
        receiver.update(now);
        now += 10;
    }

    assert_eq!(received.len(), expected.len(), "60 秒内应完成传输");
    assert_eq!(received, expected);
    assert!(!sender.is_dead());
    assert!(!receiver.is_dead());
}

/// 超时重传的包内容与 C++ 一致（C++ 的 `unordered_map` 迭代顺序不稳定，
/// 这里按 SN 排序后比较）。
#[test]
fn retransmission_matches_cpp_packets() {
    let (mut kcp, packets) = make_kcp();
    kcp.update(1000);
    kcp.send(b"hello").unwrap();
    kcp.update(1010);

    let mut ack = [0u8; OVERHEAD as usize];
    ack[..4].copy_from_slice(&0x0102_0304u32.to_le_bytes());
    ack[4] = CMD_ACK;
    ack[6..8].copy_from_slice(&128u16.to_le_bytes());
    ack[8..12].copy_from_slice(&1010u32.to_le_bytes());
    kcp.input(&ack).unwrap();
    kcp.update(1030);
    kcp.update(1040);

    kcp.set_mtu(100).unwrap();
    let big: Vec<u8> = (0..200u16).map(|value| value as u8).collect();
    kcp.send(&big).unwrap();
    kcp.update(1050);
    kcp.update(1250);

    let mut packets = packets.lock().unwrap().clone();
    // 丢掉首次发送的 4 个包，只保留 1250ms 的重传
    packets.drain(..4);
    let mut actual: Vec<String> = packets.iter().map(hex::encode_upper).collect();
    actual.sort();

    let mut expected = vec![
        concat!(
            "0403020151008000E2040000030000000000000030000000",
            "98999A9B9C9D9E9FA0A1A2A3A4A5A6A7A8A9AAABACADAEAF",
            "B0B1B2B3B4B5B6B7B8B9BABBBCBDBEBFC0C1C2C3C4C5C6C7"
        ),
        concat!(
            "0403020151018000E204000002000000000000004C000000",
            "4C4D4E4F505152535455565758595A5B5C5D5E5F606162636465666768696A6B",
            "6C6D6E6F707172737475767778797A7B7C7D7E7F808182838485868788898A8B8C",
            "8D8E8F9091929394959697"
        ),
        concat!(
            "0403020151028000E204000001000000000000004C000000",
            "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F",
            "202122232425262728292A2B2C2D2E2F303132333435363738393A3B3C3D3E3F",
            "404142434445464748494A4B"
        ),
    ];
    expected.sort();
    assert_eq!(actual, expected);
}
