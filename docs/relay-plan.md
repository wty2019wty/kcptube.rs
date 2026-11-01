# relay（中继模式）实施方案

> M5 的第一项。本文只描述**设计**，不含代码改动；实施顺序见
> [`refactor-backlog.md`](refactor-backlog.md) 5.0：先阶段 1 公共层 → 阶段 5 前置抽取 → 再按本文 R1–R5 落地。

## 1. 结论先行

- relay 在 C++ 里是「**KCP 到 KCP 的透明中继**」：不落地 TCP/UDP 连接，只把一端的 KCP 内层帧
  原样塞进另一端的 KCP 重新分段。因此 Rust 侧**不需要**复用 client/server 的转发逻辑，
  真正要复用的只有 `KcpChannel`、配置映射、日志与保活/清理规则。
- `KcpChannel` 已经封装了「KCP + 加解密 + FEC + 可换 socket/对端」，恰好覆盖 relay 两个方向的全部需求；
  relay 落地后会顺带让目前被认为「未使用」的 `keep_alive_send_time` / `keep_alive_response_time` 变成生产代码。
- 最大的实现难点不是转发，而是**握手改写**：relay 要在两个握手通道之间来回改写 `initialise`
  负载（带宽夹取、端口范围替换），并把服务端分配的 `uid` 透传给客户端。
- 建议分 5 步交付（relay 本体 R1–R4，`--try` 为 R5），每步都能单独跑通测试；STUN/DDNS 属于 M6，不混进来。

## 2. C++ 参考实现拆解（`legacy-cpp/src/modes/relay.cpp`，2058 行）

### 2.1 数据模型

| C++ 结构 | 作用 | Rust 对应 |
|---|---|---|
| `user_settings current_settings` | 根配置 + `ingress` / `egress` 两个分段 | 现有 `UserSettings { ingress, egress }` |
| `udp_servers` | 只监听 **UDP**（relay 没有 TCP 监听） | `Vec<UdpEndpoint>` |
| `handshake_ingress_map_to_channels` | `客户端源地址 → 握手映射` | `Mutex<HashMap<SocketAddr, Arc<HandshakeEntry>>>` |
| `id_map_to_both_sides` | `conv → 数据映射`（两侧共用同一 conv） | `Mutex<HashMap<u32, Arc<RelaySession>>>` |
| `expiring_handshakes` / `expiring_forwarders` / `expiring_kcp` | 三张待回收表 | 各自的 `Mutex<Vec<...>>` / 带时间戳的 Map |
| `kcp_keepalive_ingress` / `_egress` | `KCP* → 下次保活时间` | 放进 `RelaySession` 的原子字段 |
| `listener_status_counters` / `forwarder_status_counters` | 两段式统计 | 两个 `StatusCounters` |

一个 `kcp_mappings` 同时持有 `ingress_kcp`（面向客户端，conv 0 时是握手，之后是数据）
与 `egress_kcp`（面向真实服务端），两侧 **conv 相同**（都用服务端分配的 `uid`）。

### 2.2 启动流程（`relay_mode::start`，`relay.cpp:20`）

1. 若 `egress->destination_dnstxt` 非空 → 查 DNS TXT 覆盖目标（M6）。
2. 计算监听地址列表：`listen_ports` × `listen_on`；`listen_on` 为空时按 `ip_version_only`
   选 `v4()` / `v6()` 通配；非空且 `ip_only == not_set` 时把 IPv4 地址映射成 `v4_mapped` IPv6。
3. 逐个 `udp_server` 绑定，失败只记日志并整体返回失败。
4. 启动定时器：`find_expires`（1s）、`expiring_kcp`（1s）、`keep_alive_ingress` / `keep_alive_egress`（1s，仅 `keep_alive > 0`）、`stun`（M6）、`status_log`（`gbv_logging_gap`）。

### 2.3 握手时序

```
client                    relay(ingress)              relay(egress)              server
  |--- initialise(conv0) ------->|                            |                      |
  |                              | 建 ingress 握手 KCP(conv0)  |                      |
  |                              | 夹取带宽 → 建 egress 握手   |                      |
  |                              |------ initialise(conv0) --------------------------->|
  |                              |                            |<---- initialise(uid) -|
  |                              |<-- initialise(uid) --------|                      |
  |                              | 建两侧数据通道(conv=uid)     |                      |
  |<--- initialise(uid) ---------|                            |                      |
  |=========== raw_data(conv=uid) ==========> 转发 ==========> 发往 server ==========|
```

关键规则（`relay.cpp:369` / `611`）：

| 方向 | 帧 | 处理 |
|---|---|---|
| 客户端 → relay | `initialise` | 解析 `SettingsWrapper`；`inbound/outbound_bandwidth` 与 `egress` 上限取小；改写后转发给真实服务端 |
| 客户端 → relay | `test_connection`（与 `keep_alive` 同值） | **新**握手条目上由 relay 本地回一个 `test_connection`；已存在的条目转发给服务端 |
| 服务端 → relay | `initialise`（conv 0） | 以 `uid` 建两侧数据通道；`port_start/port_end` 若 ingress 端口连续则替换为 ingress 自己的端口范围；带宽与 `ingress` 上限取小；改写后转发给客户端 |
| 双向数据 | `raw_data` / `mux_transfer` / `mux_cancel` / `pre_connect_custom_address` | 原样 `Send` 到另一侧 |
| 双向控制 | `keep_alive` | 在本侧回 `keep_alive_response`，不转发 |
| 双向控制 | `keep_alive_response` | 记录 `keep_alive_response_time` |
| 双向控制 | `failure` / `disconnect` | 转发到另一侧并 `Refresh()`，同时把会话加入 `expiring_kcp` |

新数据通道的 KCP 参数：两侧分别取 `ingress` / `egress` 分段的 `kcp_mtu`、窗口、`kcp_*` 预设，
`RxMinRTO = 10`，带宽取各自分段的 `outbound/inbound_bandwidth`；
两侧都 `keep_alive_send_time = keep_alive_response_time = now`。
首个数据包被服务端确认后，用握手通道的 `GetRxSRTT()` 对两侧 `ResetWindowValues()`（`relay.cpp:1048`）。

### 2.4 数据面

- 入口：`udp_listener_incoming` → 解密 → `unpack` → 时间窗校验 → 按 conv 分派。
- conv == 0 → `udp_listener_incoming_new_connection`（握手）。
- conv != 0 → `id_map_to_both_sides[conv]` → **每包更新 `ingress_source_endpoint`**
  （客户端端口跳跃会换源地址）→ 检查对侧 `WaitQueueIsFull()` → `Input` → `Receive` 循环 → 转发。
- 出口：每个连接一条独立 forwarder socket（`udp_forwarder_incoming`），
  收到报文后 `verify_kcp_conv` 校验 conv，再 `Input` → `Receive` → 转发回 ingress。
- FEC：两个方向各自一套 `fec_control_data`，用 `fec_find_missings` 恢复后再 `Input`；
  发送侧 `fec_maker_via_*` 按 `fec_original_packet_count` 攒够后编码冗余包。

### 2.5 生命周期（`relay.cpp:1658`–`1872`）

| 清理项 | 规则 | 常量 |
|---|---|---|
| 握手条目 | 建立后 `4s` 清空输出回调、`8s` 出表 | `KCP_CLEANUP_WAITS = 4` / `×2` |
| forwarder | `4s` 停回调，`8s` 出表 | `RECEIVER_CLEANUP_WAITS` |
| 数据会话 | `disconnect` 后 `keepalive_timeout + 4s` 才回收 | `KEEPALIVE_TIMEOUT = 31` |
| 数据会话 | `min(最后活动, 保活差值) > 31 + keep_alive`（有保活）或 `最后活动 > 31`（无保活） | 同上 |
| 数据会话 | 任一方向 `> 255s` 强制回收 | `TIME_GAP_SECONDS` |
| 数据会话 | 三条件同时超过 `egress->udp_timeout`：全局最后收、ingress 最后收、egress 最后收 | `udp_timeout` |

### 2.6 端口跳跃（`relay.cpp:811`–`914`）

- `data_sender_via_listener` / `_via_forwarder` 每次发完都调用 `change_new_port`。
- `hopping_timestamp` 到期后：`hopping_available` 为真 → `switch_new_port`；
  否则若无进行中的探测 → `test_before_change`（新建一条 conv 0 通道发 `test_connection`）。
- 探测成功 → `hopping_available = true`，记录新目标 endpoint；下一次发送时切换 forwarder socket。
- 客户端侧（面向 relay 的 ingress）也会跳跃：老源地址的报文继续投递到同一 conv，
  relay 只是更新 `ingress_source_endpoint`（Rust 用 `KcpChannel::set_peer` / `rebind`）。

**间隔取自哪个配置**：`create_kcp_bidirections` 读的是 `egress->dynamic_port_refresh`
（`relay.cpp:1012`），而 `copy_settings` 恰好不复制这个字段；`check_settings` 末尾又会递归调用
`check_settings(ingress)` / `check_settings(egress)`（`configurations.cpp:791`–`804`），
所以两段各自按 `-1→60 / 0→关闭 / <20→20` 独立归一化。实际效果：

| 配置写法 | 根的值 | `[forwarder]` 的值 | relay 实际跳跃间隔 |
|---|---|---|---|
| 不写 `dport_refresh` | 60 | 60 | 60 秒 |
| 根写 30 | 30 | 60 | **60 秒**（根不生效） |
| 根写 0 | 0 | 60 | **60 秒**（根关不掉） |
| `[forwarder]` 写 0 | 60 | 0 | 关闭 |
| `[forwarder]` 写 5 | 60 | 20 | 20 秒 |

即：**relay 的端口跳跃只认 `[forwarder]` 段，根上的 `dport_refresh` 对 relay 没有作用。**
Rust 侧的具体取舍见 §9 Q2。

### 2.7 状态日志（`relay.cpp:2009`）

```
[时间]Summary of <config>
[Client <-> This] receive (raw): ..., receive (inner): ..., send (inner): ..., send (raw): ..., fec recover: ...
[This <-> Remote] receive (raw): ..., receive (inner): ..., send (inner): ..., send (raw): ..., fec recover: ...
[Client <-> This] KCP#<conv> average latency: <srtt> ms
[This <-> Remote] KCP#<conv> average latency: <srtt> ms
```

### 2.8 STUN / DDNS（M6，不在本方案）

`send_stun_request` / `save_external_ip_address` / `destination_dnstxt` 都依赖 M6 的 STUN 与 DNS TXT 实现，
R1–R4 只保留接口位，不实现行为。

### 2.9 C++ 侧已发现的复制粘贴缺陷

这些是**上游 bug**，Rust 不应照抄；但必须在 `docs/progress.md` 登记为「有意偏离」：

| 位置 | C++ 写法 | 影响 | Rust 处理 |
|---|---|---|---|
| `relay.cpp:1743` | `egress_kcp_ptr = kcp_mappings_ptr->ingress_kcp;`（应为 `egress_kcp`） | egress 的输出回调不会被清空，`kcp_updater.remove` 对同一对象调用两次 | 两侧分别清理 |
| `relay.cpp:2017` | `listener_fec_recovery = forwarder_status_counters.fec_recovery_count` | ingress 的 FEC 恢复数被记到 forwarder 段 | 用各自计数器 |
| `relay.cpp:950` / `1005` | 两次写 `kcp_ptr_ingress->keep_alive_send_time` | egress 的保活基准时间永远是构造值 | 两侧分别初始化 |
| `create_kcp_bidirections` | 用 `egress->dynamic_port_refresh` 决定首次跳跃间隔；`copy_settings` 不复制该字段，但 `check_settings` 会**递归**对 `[forwarder]` 段做一次独立归一化（默认 60） | 根上的 `dport_refresh` 对 relay 完全无效：设 30 不生效，设 0 也关不掉 | 见 §9 Q2 |

## 3. Rust 侧复用面与缺口

### 3.1 直接复用

| 能力 | 位置 | 说明 |
|---|---|---|
| KCP + 加解密 + FEC + 换 socket/对端 | `crates/transport/src/channel.rs` | 两侧各一个 `Arc<KcpChannel>` 即可 |
| 握手通道建立（独占 socket） | `transport::open_handshake_channel` | egress 侧握手 |
| 保活发送 | `transport::KeepAlive`（`new` / `tick_at`） | 两侧独立保活 |
| 端口探测与重绑 | `transport::PortHop` | R3 的 egress 跳跃 |
| 帧解析与时间窗 | `transport::PacketCodec::{open_frame, seal}`、`parse_frame`、`IncomingFrame::conv()` | 与 `server/src/socket.rs` 同一套 demux 写法 |
| 目标解析 | `transport::resolve_endpoints_filtered` | 支持 IP 字面量与域名、地址族过滤 |
| 配置映射 | `config::UserSettings`（阶段 1 会补 `kcp_config` / `fec_parameters`） | 两侧 KCP 参数直接来自分段 |
| 统计与日志 | `kcptube_logging::{StatusCounters, to_speed_unit, print_status_to_file, print_message_to_file, time_string_with_brackets}` | 状态摘要建议在阶段 1 抽成公共函数 |
| 常量 | `kcptube_protocol::constant::*` | 清理/保活/超时常量都已存在 |
| 端口探测 | `client/src/port_hop.rs`（阶段 5 泛化） | R3 复用 |

### 3.2 必须新增

1. `Relay::new` 需要**根配置**：当前 `main.rs:148` 只传了 `ingress` / `egress`，
   而 `log_status`、`log_messages`、`log_ip_address`、`config_filename` 都在根上。
   → 改为 `Relay::new(root, ingress, egress)`，或让 `Relay` 直接持有 `UserSettings`。
2. `relay/src/` 的模块拆分（见 §4.1）。
3. `SettingsWrapper` 的**原地改写**辅助（只覆盖 26 字节头，保持负载长度不变），
   避免 `decode` + `encode` 改变 KCP 消息长度。
4. 目标选择：随机选地址 + 随机选端口（`randomly_pick_index` + `generate_new_port_number` 的等价物）。
5. relay 自己的状态摘要（两段式）。

### 3.3 不要复用

- `server` 的 `DataSession` / `SessionKind`：它绑定的是「本地 TCP/UDP 落地」语义，relay 没有落地端。
- `client` 的 `tcp.rs` / `udp.rs` 转发循环：relay 的「上层」是另一条 KCP，不是 socket。
- mux 的任何逻辑：relay 对 mux 帧完全透明，**不需要** `MuxTunnel`。这是 relay 比 client/server 简单的地方。

## 4. 目标设计

### 4.1 模块划分

```
crates/relay/src/
  lib.rs        Relay 入口：校验配置、构造 RelayState、启动监听与维护任务
  state.rs      RelayState / HandshakeEntry / RelaySession / 计数器
  listener.rs   每个监听 socket 的收包循环 + conv 分派
  handshake.rs  conv==0：ingress 握手条目、向 egress 发起握手、initialise 改写与回传
  session.rs    数据会话任务：egress socket 收包、5ms 驱动、双向转发
  forward.rs    drain 内层帧 + 控制帧分派 + 转发到对侧
  target.rs     目标地址/端口选择与重解析
  port_hop.rs   R3：relay 侧端口跳跃
  status.rs     状态摘要
  util.rs       小工具
```

### 4.2 核心数据结构

```rust
pub struct Relay {
    settings: Arc<UserSettings>, // 根：log_*、fib_*、config_filename
    ingress: Arc<UserSettings>,
    egress: Arc<UserSettings>,
}

struct RelayState {
    settings: Arc<UserSettings>,
    ingress: Arc<UserSettings>,
    egress: Arc<UserSettings>,
    ingress_codec: Arc<PacketCodec>,
    egress_codec: Arc<PacketCodec>,
    ingress_fec: Option<(u8, u8)>,
    egress_fec: Option<(u8, u8)>,
    handshakes: Mutex<HashMap<SocketAddr, Arc<HandshakeEntry>>>,
    sessions: Mutex<HashMap<u32, Arc<RelaySession>>>,
    targets: RwLock<Vec<String>>,        // 可被 DDNS 刷新（M6）
    target_ports: RwLock<Vec<u16>>,
    listener_counters: StatusCounters,
    forwarder_counters: StatusCounters,
    next_status_at: AtomicI64,
}

struct HandshakeEntry {
    peer: SocketAddr,
    ingress: Arc<KcpChannel>,            // conv 0，共享 listener socket
    egress: Mutex<Option<Arc<KcpChannel>>>, // conv 0，独占 socket
    protocol: AtomicU8,
    created_at: i64,
    established: AtomicBool,
    egress_peer: Mutex<Option<SocketAddr>>,
}

struct RelaySession {
    conv: u32,
    protocol: ProtocolType,
    ingress: Arc<KcpChannel>,            // 面向客户端
    egress: Arc<KcpChannel>,             // 面向真实服务端
    last_activity: AtomicI64,
    expire_at: AtomicI64,
    next_keep_alive_ingress: AtomicI64,
    next_keep_alive_egress: AtomicI64,
    closed: AtomicBool,
}
```

### 4.3 任务模型

```
listener socket × N
  └─ run_listener
       ├─ conv == 0 → HandshakeEntry(peer)
       └─ conv != 0 → sessions[conv]
             ├─ 若源 socket/peer 变化 → rebind / set_peer
             ├─ ingress.accept_frame(frame)
             └─ drain(ingress) → 转发到 egress

relay 启动时还有：
  run_session(每个数据会话)
      select!
        ├─ egress.socket().recv_from   → egress.accept_frame + drain(egress)
        ├─ ticker 5ms                  → ingress.update / egress.update + drain 双向
        └─ closed                       → 退出
  run_maintenance（全局，1s）
        ├─ 握手条目过期（4s 清回调 / 8s 出表）
        ├─ forwarder 过期（4s 停 / 8s 出表）
        ├─ 数据会话空闲判定与回收
        ├─ keep_alive 发送（两侧独立）
        └─ maybe_log_status
```

选择「每会话一个 5ms ticker」而不是「全局扫描」，是为了让会话之间互不阻塞、也方便 R4 之后换成
deadline 驱动；代价是唤醒次数随会话数线性增长（与 C++ 的 `KCPUpdater` 相比是已知差距，
阶段 4.1 统一处理，relay 届时直接复用 `KcpUpdater`）。

### 4.4 握手中继细则

1. 新 `peer` 的 conv 0 报文 → 建 `HandshakeEntry`：
   `KcpChannel::new_with_fec(ingress_codec, handshake_config(ingress.kcp_mtu), listener_socket.clone(), peer, ingress_fec)`，
   再 `set_blast` / `set_bandwidth` / `set_rx_minrto(10)`（阶段 1 的 `apply_handshake_tuning`）。
2. 收到 `initialise`：解码 `SettingsWrapper` → 夹取带宽 → 用「只改 26 字节头」的辅助改写 →
   若还没有 egress 握手通道：
   - 选目标（随机地址 + 随机端口），`resolve_endpoints_filtered` 取首个地址；
   - `UdpEndpoint::bind(wildcard_addr(target))`；
   - 建 egress 握手 `KcpChannel`（conv 0，`egress_codec`，`egress_fec`，`egress.kcp_mtu`）；
   - 把改写后的 `initialise` 发给真实服务端。
3. 收到 `test_connection`：未 established 时本地回 `create_test_connection_packet()`；已 established 时转发。
4. egress 握手通道收到 `initialise`（`uid != 0`）：建 `RelaySession`
   （两侧 `UserSettings::kcp_config(conv)` + `reset_window_values(ingress_srtt)` + `set_rx_minrto(10)`），
   把响应改写后发回客户端：
   - `port_start/port_end`：ingress 的 `listen_ports` 连续时替换为该范围；
   - 带宽与 `ingress` 上限取小；
   - `uid` 保持服务端分配值。
5. 握手条目在数据通道建立后保留到 `8s`，用于接收迟到的握手包。

### 4.5 数据转发

```rust
async fn drain_and_forward(from: &KcpChannel, to: &KcpChannel, session: &RelaySession) {
    loop {
        let size = match from.receive(&mut buf).await { ... WouldBlock => break, ... };
        let (feature, protocol, body) = unpack_inner(&buf[..size])?;
        match feature {
            Feature::RawData | Feature::MuxTransfer | Feature::MuxCancel
                | Feature::PreConnectCustomAddress => {
                    to.send(&buf[..size]).await?;
                    session.last_activity.store(now_secs());
                }
            Feature::KeepAlive => {
                from.send(&create_keep_alive_response_packet(protocol)).await?;
            }
            Feature::KeepAliveResponse => from.mark_keep_alive_response(now_secs()),
            Feature::Failure | Feature::Disconnect => {
                to.send(&buf[..size]).await?;
                // 对应 C++ `process_disconnect`：expire_at = now + KEEPALIVE_TIMEOUT，
                // 再由清理任务等到 `+ KCP_CLEANUP_WAITS`，合计约 35 秒
                session.expire_at.store(now_secs() + KEEPALIVE_TIMEOUT);
            }
            // 数据面其它 feature（初始握手已在 conv 0 阶段处理）
            _ => {}
        }
    }
}
```

**注意**：转发的是「内层完整数据层」（含 `data_layer` 头），与 C++ `kcp_ptr->Send(buffer_ptr, kcp_data_size)` 一致，
不是 `body`。

### 4.6 端口跳跃（R3）

- 触发点：每次成功发送后检查 `hopping_at`；到期则探测。
- 探测：新建 `UdpEndpoint` + conv 0 `KcpChannel`，发 `test_connection`，3 秒内收到响应即成功。
- 切换：`session.egress.rebind(new_socket, new_peer)`，旧 socket 继续收 `RECEIVER_CLEANUP_WAITS / 2` 秒。
- ingress 侧不主动跳跃，只跟随客户端 `set_peer` / `rebind`。

### 4.7 配置与入口

- `crates/kcptube/src/main.rs:148` 改为把根 `settings` 一并传入。
- `config` 已校验：relay 必须有 `[listener]` + `[forwarder]`，`mux_tunnels` 不得设置。
- relay 只监听 UDP；`listen_port` 缺省校验已由 `verify_server_listen_port`（`RelayIngress`）覆盖。
- `fib_ingress` / `fib_egress`：M5 的 FreeBSD 平台项未完成前只记录日志，不做绑定。

## 5. 与 C++ 的对齐清单

| 项 | 对齐方式 |
|---|---|
| 线格式 | 不改动：`initialise` 负载只改固定头字段，长度不变 |
| conv | 两侧都用服务端分配的 `uid` |
| 握手超时 | 30 秒（`HANDSHAKE_TIMEOUT`） |
| 空闲判定 | 完全按 `loop_find_expires` 的四条规则 |
| 保活 | 两侧独立，`keep_alive > 0` 才启动 |
| 清理节奏 | 4s / 8s / 31s / 255s，与 C++ 常量一致 |
| 状态日志 | 两段式 + 每 conv 两行；格式逐字一致 |
| 端口跳跃 | 见 §9 待确认 |
| 统计口径 | `receive (raw)` 用收到的密文长度；`send (raw)` 用 `KcpChannel::take_output_bytes()`（密文长度），与 C++ 的「加密前长度」略有差异，仅影响日志 |

## 6. 分步实施

| 步骤 | 内容 | 预估 | 验收 |
|---|---|---|---|
| **R0** | 阶段 1 公共层（`kcp_config` / `fec_parameters` / `set_rx_minrto` / `apply_*_tuning` / `wildcard_addr` / `log_warning` / 状态摘要格式化） | ✅ 已完成 | 见 `refactor-backlog.md` 阶段 1 |
| **R0.5** | 阶段 5 前置（`PortHop` / `KeepAlive` / `open_handshake_channel`） | ✅ 已完成 | 见 `refactor-backlog.md` 阶段 5；空闲清理随 R2 |
| **R1** | relay 骨架：`Relay::new(root, ingress, egress)`、监听绑定、单 listener demux、握手中继、数据会话、双向转发、控制帧 | ✅ 已完成 | Rust client → relay → Rust server 的 TCP/UDP echo 通过 |
| **R2** | 生命周期：握手/forwarder/会话清理、保活、状态日志、错误日志 | ✅ 已完成 | 空闲回收、`keep_alive` 用例通过 |
| **R3** | 端口跳跃（relay 侧）+ 与客户端跳跃的联动 | ✅ 已完成 | 小 `dport_refresh` 下长连接不断 |
| **R4** | mux / 自定义映射 / 多目标地址透传验证 | 1 天 | 现有 mux interop 改为经 relay |
| **R5** | `--try`（tester） | 1–2 天 | 见 §7.2 |

每个步骤单独提交，提交前执行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 7. 测试计划

### 7.1 relay

| 编号 | 用例 | 方式 |
|---|---|---|
| T1 | client → relay → server，TCP echo 往返 | `crates/server/tests/interop.rs` 新增 relay 变体 |
| T2 | 同上，UDP echo | 同上 |
| T3 | 带 FEC（`fec=10:3`）经 relay | 同上 |
| T4 | `keep_alive` 经 relay 续命超过 255 秒的空闲阈值 | 缩短常量或用 31s 判定 |
| T5 | mux（`mux_tunnels=2`）经 relay，多连接并发 | 复用现有 100 并发用例 |
| T6 | 客户端端口跳跃（`dport_refresh=2`）后连接不断 | 需要 R3 |
| T7 | 客户端先断开，relay 会话按 35 秒规则回收 | 单元测试 + 日志断言 |
| T8 | relay 侧目标不可达 → 回 `failure`，会话释放 | 单元测试 |
| T9 | `initialise` 改写：带宽夹取、端口范围替换 | 纯函数单元测试 |
| T10 | 与 C++ 客户端 / 服务端互通 | 需 C++ 二进制，作为人工验收项 |

### 7.2 `--try`（tester）

- 输入：client 配置或 relay 配置（relay 用 `[forwarder]` 分段）。
- 行为：对「每个目标地址 × 每个目标端口」并发建 conv 0 握手，发 `test_connection`，30 秒超时。
- 输出（逐字对齐 C++ `PrintResults`）：

```
Connection Test Result #0 of "example.com":
Selected IP Address: 1.2.3.4
Success: ALL (5)
Failure: NONE
```

- 实现建议：新增 `crates/tester`（`kcptube-tester`），复用 `resolve_endpoints_filtered` + `KcpChannel`；
  不需要 `KcpUpdater`，每个探测一个独立任务即可。

## 8. 风险与回滚

| 风险 | 缓解 |
|---|---|
| `initialise` 改写导致线格式不一致 | 只改 26 字节头；加字节级单元测试（构造 → 改写 → 逐字节比对） |
| 单 listener 循环被慢会话阻塞 | demux 只做 `accept_frame`（锁内极短）+ 转发；转发失败只记日志 |
| 端口跳跃改变源端口导致服务端重建会话 | 保留旧 socket 继续收 4 秒；服务端本来就按 conv 迁移 peer |
| 空闲判定与 C++ 不一致导致连接被提前断开 | 直接用同一组常量并写单元测试覆盖四条规则 |
| relay 引入第三份复制 | 先做 R0 与阶段 5 前置；relay 不复制 client/server 的转发循环 |

回滚：relay 是独立 crate，`main.rs` 只有一处分支；出问题可整体回退到 `NotImplemented` 而不影响 client/server。

## 9. 待确认

| # | 问题 | 建议 |
|---|---|---|
| Q1 | relay 的端口跳跃是否第一版就做？ | 建议 R3 再做，R1/R2 先不做，避免一次引入两个变量 |
| Q2 | relay 的跳跃间隔只认 `[forwarder]` 段自己的 `dport_refresh`，根配置的同名项不生效（设 30 无效、设 0 关不掉） | 建议分段未显式设置时**继承根配置归一化后的值**；在 `progress.md` 登记为有意偏离 |
| Q3 | §2.9 的四个 C++ 缺陷是否照抄？ | 建议修掉并登记；它们不影响线格式 |
| Q4 | `--try` 是否与 relay 同批交付？ | 建议 R5 单独交付，互不阻塞 |
| Q5 | 是否需要 `crates/tester` 新 crate？ | 建议新建；比塞进 relay 更清晰，也便于以后扩展 |

## 10. 变更记录

| 日期 | 内容 |
|---|---|
| 2026-11-01 | 建立本文档：C++ relay 行为拆解、Rust 复用面与缺口、目标设计与 R0–R5 分步实施、测试计划与待确认项 |
| 2026-11-01 | R0 与 R0.5 完成：`transport` 新增 `PortHop` / `KeepAlive` / `open_handshake_channel`，§3.1 复用面同步更新；空闲清理留到 R2 |
| 2026-11-01 | R1 完成：`crates/relay` 的 `listener` / `handshake` / `session` / `state` / `util` 落地，握手改写与双向转发可用；`Relay::new` 改为接收根配置；新增 `relay_tcp_roundtrip` / `relay_udp_roundtrip`。实现细节与设计差异：listener 只做解密与 conv 分派、把明文帧投递给会话任务（不在监听循环里驱动 KCP）；`listen_on` 不复刻 C++ 的 v4-mapped 映射（已登记 `progress.md` 已知问题 #1.1）；空闲清理/保活/状态日志/端口跳跃仍属 R2/R3 |
| 2026-11-01 | R2 完成：`relay/maintenance` 每秒执行空闲判定（255 秒强制、`udp_timeout` 三条件）与两侧独立保活；`relay/status` 输出两段式摘要；C++ 的 FEC 计数复制粘贴缺陷（§2.9 第 2 条）已修正；新增 `relay_keep_alive_stays_connected`。C++ 的 `expiring_handshakes` / `expiring_forwarders` / `expiring_kcp` 三张表在 Rust 侧由任务自行收尾（会话任务退出后保留 4 秒冲刷），因此不需要独立清理表 |
| 2026-11-01 | R3 完成：`relay/port_hop` 在每次转发后按 `[forwarder].dport_refresh` 探测并切换 egress socket，复用 `transport::PortHop` 与 `drain_old_socket`；`RelaySession.egress_socket` 改为可替换。与 C++ 的两处有意偏离：探测失败按 `dport_refresh` 重试（C++ 一次失败即永久停止）、探测成功立即切换（C++ 需下一次发送）；新增 `relay_port_hopping_keeps_connection` |
