# kcptube Rust 移植进度

> 最后更新：2026-11-01
> 分支：`rs-dev`　基线提交：`a0d7164`（legacy-cpp 迁移 + .gitignore）
> 计划文档：[`docs/dev/migration-plan.md`](migration-plan.md)

---

## 1. 总览

| 指标 | 数值 |
|---|---|
| C++ 参考实现 | 14,308 行（`legacy-cpp/`，含 ikcp 1,109 行） |
| Rust 代码 | 14,114 行（55 个源文件，非空行，含 `#[cfg(test)]` 模块） |
| crate 数 | 12（11 个 lib + 1 个 bin） |
| 单元/集成测试 | 149 个，全部通过 |
| 整体完成度 | **M0~M4 完成，M5 进行中** |

```
M0 基线            ██████████ 100%
M1 协议基础层      ██████████ 100%
M2 KCP/传输        ██████████ 100%
M3 TCP 最小互通     ██████████ 100%
M4 完整模式         ██████████ 100%
M5 relay/跨平台     █████████▌  95%   ← relay R1–R4、--try、T4/T10 验收完成
M6 STUN/DDNS       ░░░░░░░░░░   0%
```

---

## 2. 各模块状态

| 模块 | C++ 对应 | 状态 | 说明 |
|---|---|---|---|
| `crates/protocol` | `connections.hpp` 包结构、`share_defines.hpp` 常量 | 🟢 100% | 常量、枚举、包层/数据层/mux/settings_wrapper 编解码全部完成并有测试 |
| `crates/crypto` | `aead.hpp`、`data_operations.cpp`、`simple_hashing.hpp` | 🟢 100% | 4 种 AEAD（`aes-gcm`/`aes-ocb`/`chacha20`/`xchacha20`）与 `xor`/`none` 的 CRC32 全部与 Botan 向量逐字节一致（含 `chacha20` 8B IV 的原始版 AEAD） |
| `crates/fec` | `data_operations.cpp` + Botan ZFEC | 🟢 100% | ZFEC 矩阵/GF(2^8)/编解码已移植，share 与 Botan 3.13 逐字节一致；含 `fec_container` 打包 |
| `crates/kcp` | `3rd_party/ikcp.cpp`、`networks/kcp.cpp` | 🟢 100% | ikcp 全量移植；发送/重传/拥塞/窗口探测与 C++ 输出逐字节对撞通过 |
| `crates/config` | `configurations.cpp`（1010 行） | 🟢 100% | 两遍解析、预设展开、MTU、relay 分段、自定义映射、日志路径、错误文本全部对齐 |
| `crates/transport` | `connections.cpp`、`kcp_updater.cpp`、`mux_tunnel.cpp` | 🟢 100% | KCP 调度、加密包层、UDP/TCP 封装、`KcpChannel`、FEC 收发流水线、mux 隧道、可切换发送 socket |
| `crates/client` | `client.cpp`（1917 行） | 🟢 100% | TCP + UDP 监听、握手（超时换端口、多地址逐个尝试、ICMP 错误不中断）、`raw_data`/mux 转发、端口跳跃、随机目标选择、保活、状态日志；`[custom_input*]` 映射与 `pre_connect_custom_address` 已接入 |
| `crates/server` | `server.cpp`（1323 行） | 🟢 100% | 端口范围监听、TCP/UDP/mux 转发、端口跳跃重定向、会话回收、保活、状态日志、`ignore_destination_*` 目标；STUN/DDNS 移至 M6 |
| `crates/logging` | `share_defines.cpp` 日志函数 | 🟢 100% | messages（追加）/ status（覆盖）两类文件已接入，ip 文件接口预留（随 M6）、`to_speed_unit` 与 C++ 逐档一致 |
| `crates/relay` | `relay.cpp`（1802 行） | 🟢 100% | R1–R4 完成：ingress 监听、两侧握手改写、双向转发、空闲回收、两侧保活、两段式状态日志、egress 端口跳跃、mux/FEC/自定义映射验证；2026-11-01 修复保活从不发送、`disconnect` 立即回收、目标解析失败不回 `failure`；已完成 T4（300 秒空闲）与 T10（C++ 桌面原版四场景）人工验收 |
| `crates/kcptube` | `main.cpp` | 🟢 100% | clap 参数、usage、`--check-config`、`--try`（`kcptube-tester`）均已接入 |
| `--try` 连通性测试 | `tester.cpp`（392 行） | 🟢 100% | `kcptube-tester`：对「每个地址 × 每个端口」并发探测，输出与 C++ `PrintResults` 逐字一致 |

---

## 3. 已完成项

### 3.1 环境与工具链（M0）

- [x] Rust 1.96（`x86_64-pc-windows-gnu`），依赖源：官方 crates.io
- [x] MSYS2 GCC 16.2 + CMake 4.4.5 + asio 1.34 + Botan 3.13
- [x] C++ 参考实现编译成功并能运行（`kcptube version 20260329`）

### 3.2 工程骨架（M0）

- [x] workspace（`resolver = 3`，edition 2024）
- [x] 10 个 crate 的目录、`Cargo.toml`、模块划分
- [x] 共享依赖清单（`[workspace.dependencies]`）
- [x] `.gitignore` 增加 `target/`
- [x] CI：fmt / clippy（`-D warnings`）/ test，ubuntu + windows 矩阵
- [x] 新 `README.md`（Rust 版说明），原文档在 `legacy-cpp/`

### 3.3 协议基础层（M1）

- [x] **protocol**：`packet_layer` / `packet_layer_data` / `packet_layer_fec` / `data_layer` / `mux_data_wrapper` 编解码
- [x] **protocol**：`settings_wrapper`（26 字节头 + NUL 结尾 IP，线上 27 字节起）与 `pre_connect_custom_address`
- [x] **protocol**：`initialise` / `keep_alive` / `keep_alive_response` / `disconnect` / `failure` / `raw_data` / `mux_*` 包构造与解析（含 `--try` 使用的 `test_connection`）
- [x] **crypto**：`chacha20` 8 字节 nonce 与 Botan 对齐（原始版 draft-agl AEAD，非 RFC 8439）
- [x] **crypto**：`xor` / `none` 的 CRC32 校验字节序修正（Botan `store_be`）
- [x] **fec**：GF(2^8) 表、`create_inverted_vdm`、编码矩阵、`invert_matrix`、编解码、`fec_container`
- [x] **config**：完整解析器（两遍解析、范围/列表、`->` 映射、kcp 预设、relay 分段、MTU、日志路径）
- [x] **config**：错误文本与 C++ 版逐字对齐
- [x] 测试：4 份样例配置全部通过 `--check-config`

### 3.4 向量对撞结果

**加密**（key = `[0x01;32]`，IV = `[0x02;N]`，AAD = `"KCP PortHopping"`，明文 `hello kcptube!!`）：

| 模式 | IV 长度 | Botan 输出（前 16 字节） | Rust | 结论 |
|---|---:|---|---|---|
| `aes-gcm` | 16 | `1853FB26B0E14477A…` | 相同 | ✅ |
| `aes-ocb` | 12 | `C581B50BAB10C402F…` | 相同 | ✅ |
| `xchacha20` | 24 | `A4035A83C557972ED…` | 相同 | ✅ |
| `chacha20` | 8 | `C80B52B63083322CD6C4169457B3C24ACFDD51C9A9C845910F8DFC7F90AD1A` | 相同 | ✅ |

> `chacha20` 的旧记录 `C80B52B6…F81C62` 已作废；用 `botan cipher` 重新生成的完整向量写入
> `crates/crypto/src/cipher.rs::chacha20_8byte_nonce_matches_botan`。
>
> `xor` / `none` 不含 AEAD，仅做 CRC32 校验；其 2 字节大端折叠已由
> `crates/crypto/src/cipher.rs::checksum16_matches_botan_crc32` 对撞通过。

**FEC**（Botan `ZFEC(4,6)`，输入 `00 01 … 0F`）：

| share | Botan 输出 |
|---|---|
| 4 | `88 89 8A 8B` |
| 5 | `24 25 26 27` |

Rust 输出完全一致，见 `crates/fec/src/lib.rs::vectors_match_botan_zfec`。

### 3.5 KCP 与传输层（M2，已完成）

- [x] **kcp**：逐行移植 `ikcp.cpp`（send/input/receive/update/check/flush/put）
- [x] **kcp**：`resendts_buf` / `fastack_buf` 索引改用 `BTreeMap<时间, BTreeSet<SN>>`
- [x] **kcp**：`SetBandwidth` / `ResetWindowValues` / `WaitQueue*`
- [x] **kcp**：`nodelay` 保留 0/1/2 三档语义
- [x] **kcp**：与 C++ 探针同一调用序列的输出**逐字节一致**（首次发送 + 分片）
- [x] **kcp**：10% 丢包的进程内回环，40 条消息完整送达
- [x] **transport**：`KcpUpdater` 最小堆调度器（u32 时间回绕安全）
- [x] **transport**：`PacketCodec`（加密 + `packet_layer` 时间戳 + 255 秒时间窗校验）
- [x] **transport**：TCP 会话/监听封装（异步读写、背压、`into_split`）
- [x] **transport**：UDP 封装（`socket2`、`SO_REUSEADDR`、IPv6 双栈、同步 `try_send_to`）
- [x] **transport**：`KcpChannel` 完整流水线（KCP → 加密 → UDP；UDP → 解密 → KCP），本机 UDP + AES-GCM 实测通过
- [x] FEC / mux 钩子（FEC 见 3.8，mux 见 3.9）

> 已知差异：C++ 的重传顺序依赖 `std::unordered_map` 迭代顺序（不可移植），
> Rust 版固定按 SN 升序发送；报文内容与集合完全一致，仅同一次 flush 内的顺序不同。
>
> `SO_SETFIB` 是 FreeBSD 专有选项，随 M5 平台适配补充。

### 3.6 TCP 最小互通（M3，已完成）

- [x] **server**：UDP 监听（`listen_on` 下的全部 `listen_port`，M4 起支持端口范围）
- [x] **server**：`conv=0` 握手分流、`initialise` 解析、随机分配 conv
- [x] **server**：连接目标 TCP，双向 `raw_data` 转发
- [x] **server**：按数据包源地址更新回包目标（C++ 客户端握手后会切换本地 UDP 端口）
- [x] **server**：握手通道 4 秒回收；会话关闭后保留 31 秒冲刷重传再清理
- [x] **server**：空闲 255 秒强制关闭并发 `inform_error("TCP Session Closed")`
- [x] **server**：`keep_alive` 周期发送与响应时间戳
- [x] **client**：本地 TCP 监听，每个连接独立 UDP socket + 握手
- [x] **client**：握手响应解析，切换到服务端分配的 conv
- [x] **client**：握手 30 秒超时后**换新本地端口重试**（对齐 `handshake_timeout_detection`），并对解析出的每个地址依次尝试
- [x] **client**：TCP ↔ `raw_data` 双向转发，保活发送/响应
- [x] **client**：连接结束后保留 4 秒冲刷最后的 `disconnect`
- [x] **加密**：默认 `aes-gcm` 全流程
- [x] **日志**：`log_messages`（追加）、`log_status`（每 60 秒覆盖写）、`to_speed_unit` 分档与 C++ 一致
- [x] 互通验证：
  - Rust 客户端 → **C++ 服务端**：3 次 TCP echo 往返通过
  - **C++ 客户端** → Rust 服务端：3 次 TCP echo 往返通过
  - Rust 客户端 → Rust 服务端：自动化集成测试 `crates/server/tests/interop.rs`
  - 保活 + 连续重连：`keep_alive_and_reconnect`（跨 3 个保活周期 + 2 次重连）
  - 状态日志实测：`server_output.log` / `<配置名>_status.log` 按 60 秒周期写入
- [x] 握手健壮性（跨设备必需，2026-11-01 修复）：
  - 时间窗按 C++ `std::abs` 双向容忍 ±255 秒时钟偏差
  - 底层 recv 错误（ICMP 不可达）不再中断握手，由 30 秒超时统一兜底
  - 域名解析出的全部地址逐个尝试（A/AAAA 任一可用即可）
  - 未指定 IP 版本时按双栈 `[::]` 监听
- [x] 连续 1 小时稳定性测试
  - 2026-10-31 19:42 完成（Rust 客户端 ↔ Rust 服务端，`keep_alive=5`）
  - 长连接每 5 秒往返：**719 / 719 成功，0 失败**
  - 每 30 秒新建连接：**119 / 119 成功，0 失败**
  - 往返延迟：平均 4.7 ms，最大 13.2 ms
  - 期间无断流、无会话泄漏；状态日志按 60 秒稳定写入
- [ ] `ip_address.log`（依赖 STUN / DDNS，随 M6 补齐）

> C++ 参考版编译使用 `legacy-cpp/` 的临时副本（仅 `dns_helper.cpp` 做 MinGW 兼容补丁），
> 未修改仓库内的 `legacy-cpp/`。

### 3.7 UDP 转发（M4，已完成）

- [x] **server**：按 `settings_wrapper` 的协议类型分流，握手后建立目标 UDP 套接字
- [x] **server**：`raw_data(udp)` → 目标 UDP；目标回包 → `raw_data(udp)`（独立接收循环）
- [x] **server**：双向空闲超过 `udp_timeout` 才回收会话（两个方向分别计时）
- [x] **client**：同一监听端口同时绑定 TCP + UDP（对齐 C++ `udp_access_points`）
- [x] **client**：按本地 UDP 源地址分发到独立 KCP 通道（对齐 `udp_local_session_map_to_kcp`）
- [x] **client**：首个数据报先入队，握手完成后再发送（对齐 C++ 的 `udp_seesion_caches`）
- [x] **client**：回包按本地源地址发回；中继空闲 `udp_timeout` 后结束
- [x] 互通验证：
  - **C++ 客户端** → Rust 服务端：3 次 UDP echo 往返通过
  - Rust 客户端 → **C++ 服务端**：3 次 UDP echo 往返通过
  - Rust 客户端 → Rust 服务端：自动化测试 `interop.rs::udp_roundtrip`
- [x] `ignore_destination_*` 下由客户端指定目标地址（`request_initialise_packet_with_address` 已接入，见 3.11）
- [x] mux 模式下的 UDP（`mux_transfer`，见 3.9）

### 3.8 FEC 接入收发流水线（M4，已完成）

- [x] **transport**：`FecSender` —— 每段加 `packet_layer_data` 头，攒够 `k` 个后打包成 `fec_container` 并编码
- [x] **transport**：只发送冗余 share（`packet_layer_fec` 含 `sn` / `sub_sn` / `kcp_conv`）
- [x] **transport**：`FecReceiver` —— 按 `sn` / `sub_sn` 缓存，凑够 `k` 个后解码，只把缺失的段交给 KCP
- [x] **transport**：`packet_layer` / `packet_layer_data` / `packet_layer_fec` 三种帧的解析与路由
- [x] **server/client**：启用 FEC 后握手通道与数据通道统一走 FEC 帧格式（conlen 0 时只加头不冗余）
- [x] 顺带修复：KCP 消息可能远大于单个 UDP 包，收发缓冲改为 64 KiB（此前 >1419 字节的写入会报「接收缓冲区太小」）
- [x] 互通验证（`fec=10:3`，32 KiB 数据）：
  - Rust 客户端 → **C++ 服务端**：内容完全一致
  - **C++ 客户端** → Rust 服务端：内容完全一致
  - Rust ↔ Rust：`interop.rs::fec_tcp_roundtrip`
- [x] 人为丢包下的 FEC 恢复（`interop.rs::fec_recovers_under_packet_loss`，进程内丢包代理每 10 个报文丢 1 个，32 KiB 数据完整）

### 3.9 mux 多路复用（M4，已完成）

- [x] **transport**：新增 `crates/transport/src/mux.rs`
  - `MuxTunnel::run` 独占一条 KCP 通道，负责收包、`update`、保活
  - 收到的 `mux_transfer` / `mux_cancel` / `pre_connect_custom_address` 经事件队列交给上层
  - 上层发送经有界命令队列串行化，队满即背压（替代 C++ 的 `deque` + `pause()`）
- [x] **client**：后台任务维持 `mux_tunnels` 条共享通道，建链失败自动重试，本地连接按 `connection_id` 复用
- [x] **client**：TCP 接入不再新建 KCP 通道，改为分配 `connection_id` 并复用隧道
- [x] **client**：UDP 按本地源地址建立/复用记录，回包直接发回该地址
- [x] **client**：通道断开后从隧道池移除，由维护任务自动补建（对齐 `establish_mux_channels`）
- [x] **client**：没有可用隧道时本地 TCP 连接排队等待（最长 `HANDSHAKE_TIMEOUT` 30 秒），不再立即丢弃
- [x] **server**：握手 `protocol_type::mux` 建立 mux 会话，KCP 由隧道独占驱动
- [x] **server**：`mux_transfer` 首次到达时按 `destination_address` 连目标（TCP）或建 UDP 套接字
- [x] **server**：目标侧断开 → `mux_cancel`；收到 `mux_cancel` → 关闭本地连接
- [x] **server**：已取消的连接进入 4 秒保留期（`EXPIRING_WAITS`），此期间再收到数据回 `mux_cancel`
- [x] **server**：UDP 记录双向空闲超过 `udp_timeout` 回收并通知对端
- [x] **server**：`pre_connect_custom_address` 按客户端给定地址预连接
- [x] 互通验证（`mux_tunnels=3`，3 条并发 TCP + 64 KiB + UDP）：
  - Rust 客户端 → **C++ 服务端**：全部一致
  - **C++ 客户端** → Rust 服务端：全部一致
  - Rust ↔ Rust：`interop.rs::mux_tcp_and_udp_roundtrip`
- [x] mux + FEC 组合（`fec=10:3`、`mux_tunnels=2`）双向互通

### 3.10 端口跳跃与默认参数（M4，已完成）

- [x] **server**：监听 `listen_ports` 中的全部端口（连续范围与列表），所有监听 socket 共享同一份会话表
- [x] **server**：`conv` 从新端口 / 新源地址到达时，自动重绑回包 socket 与对端（对应 C++ `ingress_listener` / `ingress_source_endpoint`）
- [x] **server**：KCP 定时驱动与维护只启动一份，避免多端口重复 `update`
- [x] **client**：每个数据通道按 `dport_refresh` 周期发送 `test_connection` 探测新目标
- [x] **client**：探测成功后重绑发送 socket 与目标地址，旧 socket 继续收 4 秒后关闭
- [x] **client**：握手响应中的服务端端口范围会覆盖本地动态目标端口池（对齐 C++ `on_handshake_success`）
- [x] **client**：每次连接随机选择 `destination_address` 与 `destination_port`（对齐 C++ `randomly_pick_index` / `generate_new_port_number`）
- [x] **client**：只监听第一个 `listen_port`（对齐 C++ `listen_ports.front()`）
- [x] **`blast`**：发送后立即 flush（对应 C++ `Refresh()`）
- [x] **带宽**：握手 / 数据通道调用 `Kcp::set_bandwidth`，并用握手 SRTT 执行 `ResetWindowValues`
- [x] **`ipv4_only` / `ipv6_only`**：监听 socket 地址族、`listen_on` 校验与目标解析按配置生效
- [x] 互通验证：`interop.rs::port_hopping_keeps_connection`（`dport_refresh=1` 下跨越多次跳跃继续收发）
- [x] 服务端握手通道对 feature 3 回 `test_connection`（与 C++ `server.cpp` 一致），修复 C++ 客户端端口探测无法确认的问题
- [x] 与 C++ 双向互通：
  - Rust 客户端 → **C++ 服务端**（TCP）：Rust 客户端日志记录 2 次跳跃，跨越跳跃持续收发
  - Rust 客户端 → **C++ 服务端**（UDP）：同样跨越 2 次跳跃持续收发
  - **C++ 客户端** → Rust 服务端（UDP）：C++ 客户端空闲后触发探测，Rust 服务端记录 2 次重绑，跨越跳跃持续收发

> 与 C++ 的语义差异：C++ 的 `loop_find_expires` 会跳过 TCP 通道，且只在
> `keep_alive_timed_out`（链路空闲/疑似超时）后才尝试换端口，`dport_refresh`
> 实际是两次尝试的最小间隔；Rust 目前对所有数据通道按 `dport_refresh`
> 周期主动跳跃。两者互通无碍，但跳跃时机与协议范围不同，见已知问题。

### 3.11 自定义输入映射（M4，已完成）

- [x] **客户端**：`ignore_listen_address` / `ignore_listen_port` 为真时不再监听 `listen_on` + `listen_port`，改为逐条监听 `[custom_input]`（TCP + UDP）、`[custom_input_tcp]`（仅 TCP）、`[custom_input_udp]`（仅 UDP）映射
- [x] **客户端**：映射本地地址为空时按 `ip_version_only` 选择通配地址；`ipv4_only` 下跳过 IPv6 映射项并写日志（与 C++ `multiple_listening_tcp` / `multiple_listening_udp` 一致）
- [x] **客户端**：握手包改用 `request_initialise_packet_with_address`，把映射右侧目标地址随 `settings_wrapper` 交给服务端
- [x] **客户端**：mux 模式建记录后立即发送 `mux_tell_server_connect_address`（`pre_connect_custom_address`），让服务端在数据到达前预连接
- [x] **服务端**：`ignore_destination_address` / `ignore_destination_port` 任一为真时，TCP/UDP 目标取自握手包里的 `user_input_ip` / `user_input_port`（`target_for`）
- [x] **服务端**：mux 握手不再要求 `destination_*`（对齐 C++ `protocol_type::mux` 直接 `connect_success = true`）；忽略默认目标时，没有 `pre_connect_custom_address` 的 `mux_transfer` 直接回 `mux_cancel`
- [x] 互通验证：
  - `interop.rs::custom_input_tcp_roundtrip` / `custom_input_udp_roundtrip`
  - `interop.rs::custom_input_mux_roundtrip`（`mux_tunnels=1`，TCP + UDP）
  - `interop.rs::custom_input_ignore_destination_address`（服务端用 `destination_address={}`）
- [x] M4 验收：`interop.rs::mux_hundred_concurrent_tcp`（4 条隧道、100 并发 TCP 全部完成）

---

## 4. 测试统计

| crate | 测试数 | 覆盖内容 |
|---|---:|---|
| `protocol` | 12 | 位域、字节序、包层/FEC/mux/settings 编解码、握手头原地改写、截断输入 |
| `crypto` | 12 | 4 种 AEAD 向量、`xor`/`none` 往返、篡改检测、IV 展开、CRC32 字节序 |
| `fec` | 7 | GF 表、Botan 向量、往返、缺 share、container |
| `kcp` | 13 | C++ 逐字节 transcript、分片、ACK、RTO、10% 丢包回环、窗口/带宽 |
| `config` | 20 | 样例配置、端口/地址、预设、FEC/MTU、relay、自定义映射、IP 版本、错误文本、`kcp_config`/`fec_parameters` |
| `transport` | 31 | 调度器、加密包层、UDP/TCP 封装、双通道 UDP 端到端、FEC 编解码/恢复/清理、mux 记录表、socket 重绑、blast、带宽、地址族解析、通配地址与调优助手、保活计时器、握手通道建立、端口探测超时 |
| `logging` | 6 | `to_speed_unit` 分档、空路径、追加/覆盖写、状态摘要格式快照、告警追加 |
| `relay` | 14 | 监听地址枚举、随机目标选择、带宽夹取、端口范围改写、空闲判定、保活调度与实发、`disconnect` 延迟回收、两段式摘要格式 |
| `tester` | 3 | 结果格式快照、部分成功端口排序、模式判定 |
| `server` | 2 | 端口连续性、超时常量与 C++ 对齐 |
| `server`（集成） | 25 | `interop.rs`：TCP 往返、UDP 往返、FEC 往返、FEC 人为丢包、mux 往返、mux 启动重试、端口跳跃、保活/重连、100 并发、自定义映射（TCP/UDP/mux/`ignore_destination_address`）、relay TCP/UDP/保活/端口跳跃/客户端经 relay 跳跃/mux TCP+UDP/mux 20 并发/FEC/自定义映射/目标失败回 `failure`、`--try` 成功/失败 |
| `client` | 4 | 握手超时常量、监听项生成（常规 / `[custom_input*]` 分段 / `ipv4_only` 过滤） |
| **合计** | **149** | 全部通过 |

命令与结果：

```
cargo fmt --all -- --check        → 0
cargo clippy --workspace --all-targets -- -D warnings → 0
cargo test --workspace            → 149 passed, 0 failed
cargo run -p kcptube -- --check-config service/config_sample_*.conf → No
```

---

## 5. 已知问题与技术债

> 编号沿用历史记录：`#10` 为已关闭的版本号横幅问题，为免混淆新增条目从 `#12` 起编号。

| # | 问题 | 影响 | 计划 |
|---|---|---|---|
| 1 | ~~relay R1–R4 已完成（含 mux/FEC/自定义映射），未与 C++ 端做人工互通验收~~ | 2026-11-01 用桌面 C++ `20260329` 原版跑通 `core` / `fec` / `mux` / `reverse`（`scripts/verify-t10-relay-interop.ps1`） | 已关闭 |
| 1.1 | relay 的 `listen_on` 不做 v4-mapped 映射（C++ 在未指定 IP 版本时会把 IPv4 映射成 `::ffff:a.b.c.d`） | 显式写 IPv4 时按 IPv4 绑定；Windows 默认 `IPV6_V6ONLY=1` 下映射绑定会失败，故有意偏离 | 保持现状，文档说明 |
| 2 | ~~`--try` 仅打印“尚未实现”~~ | 已实现：新增 `kcptube-tester`，支持 client/relay 配置 | 已关闭 |
| 3 | 会话空闲 255 秒会被强制关闭（与 C++ 一致），依赖 `keep_alive` 续命 | 长时间无流量的小连接可能被断开 | 保持兼容，文档说明 |
| 3.1 | 握手通道在 4 秒后直接回收（C++ 是 4 秒清空输出、8 秒出表） | 等价行为，仅表项存在时间略短 | 保持现状 |
| 3.2 | 客户端状态日志的 `KCP#<conv>` 用服务端分配的 conv | 与 C++ 一致 | — |
| 3.3 | mux 背压用有界队列替代 C++ 的 `pause()` | 行为等价（读变慢即背压） | 保持现状 |
| 4 | 协议无版本字段，锁定 `20260329` | 上游升级需同步 | 每次上游发版做 diff 并记录 |
| 5 | `SO_SETFIB`（FreeBSD）未实现 | FreeBSD 多路由表不可用 | M5 |
| 6 | `dashmap` 在 workspace 依赖中声明但未被任何 crate 使用 | 依赖膨胀 | 移除或接入 |
| 7 | ~~`ignore_destination_*` / `[custom_input*]` 映射转发未接入；mux `pre_connect_custom_address` 发送侧待补~~ | 已修复：客户端按映射监听并随握手包下发目标，mux 模式发送 `pre_connect_custom_address`；服务端 `ignore_destination_*` 生效 | 已关闭 |
| 8 | STUN / DDNS 未实现（已从 M4 移入 M6） | 公网地址探测与动态 DNS 不可用 | M6 |
| 9 | 端口跳跃时机不同：Rust 对所有数据通道按 `dport_refresh` 周期主动跳跃；C++ 跳过 TCP，且仅在链路空闲超时后探测 | 与 C++ 的跳跃频率/触发条件不一致，可能影响 QoS 观察 | 评估是否对齐 C++ 或保留现状并文档说明 |
| 12 | ~~未指定 `ipv4_only` / `ipv6_only` 时 Rust 只监听 IPv4 通配 `0.0.0.0`~~ | 已修复：改为与 C++ 一致的 IPv6 通配 `[::]` + 双栈（`ipv6_only` 才置 `IPV6_V6ONLY`） | 已关闭 |
| 13 | 多配置文件按顺序启动：`main.rs` 逐个 `await` 各 profile，客户端/服务端首个常驻的配置不退出时后续配置不会启动（C++ 并发运行） | 多配置文件部署实际只生效第一个 | 后续改为并发启动 |
| 14 | ~~relay 的 255 秒空闲阈值只有单元测试与 5 秒保活用例，没有 255 秒级端到端长跑~~ | 2026-11-01 用 `scripts/verify-t4-relay-keepalive.ps1` 实测：客户端 `keep_alive=0`、relay `keep_alive=1`，空闲 300 秒后 TCP/UDP 仍通 | 已关闭 |

---

## 6. 下一步

1. [x] 连续 1 小时稳定性与断线重连测试
2. [x] 握手重试、超时清理、dead link 清理与 C++ 对照
3. [x] 状态日志（status / messages）格式对齐（`ip_address.log` 待 M6）
4. [x] **UDP 转发**（`udp_timeout`、UDP 映射表）
5. [x] **mux**：`mux_tunnels`、`mux_transfer` / `mux_cancel` / `pre_connect_custom_address`（含发送侧）
6. [x] **端口跳跃**：`dport_refresh`、`test_connection`、socket 切换、服务端端口范围
7. [x] FEC 接入发送/接收流水线
8. [x] 带宽限制、`blast`、`ipv4_only` / `ipv6_only`
9. [x] `ignore_destination_*` 与 `[custom_input*]` 映射（M4 收尾）
10. [x] M5 之 relay R0/R0.5：公共层抽取（阶段 1 与阶段 5 前置）
11. [x] M5 之 relay R1：握手改写与双向转发（2026-11-01 完成）
12. [x] M5 之 relay R2：空闲回收、两侧保活、两段式状态日志（2026-11-01 完成）
13. [x] M5 之 relay R3：egress 端口跳跃（2026-11-01 完成）
14. [x] M5 之 relay R4：mux / FEC / 自定义映射经 relay 验证（2026-11-01 完成）
15. [x] M5 之 relay T10：与 C++ 客户端/服务端做人工互通验收（2026-11-01 完成，桌面 C++ 原版四场景通过）
16. [x] M5 之 `--try`（tester）：新建 `kcptube-tester`（2026-11-01 完成）
16.1 [x] relay 代码验证后的收尾：保活初始化、`disconnect` 31 秒延迟回收、目标失败回 `failure`、客户端经 relay 跳跃与 `initialise` 改写单测；T4（空闲 300 秒）与 T10（C++ 四场景）验收通过（2026-11-01 完成）
17. [ ] M5 之 FreeBSD `SO_SETFIB`、性能基准与发布
18. [ ] M6：STUN、DDNS、`ip_address.log`

完成标准：M4 验收表中的 UDP、mux、端口跳跃、自定义映射全部可用；STUN/DDNS 转入 M6。

---

## 7. 变更记录

| 日期 | 内容 |
|---|---|
| 2026-10-31 | 完成 M0：C++ 代码迁入 `legacy-cpp/`；建立 Rust workspace 骨架；加密 4 种模式与 Botan 向量对撞（3 种完全一致）；CI 落地；编写计划与进度文档 |
| 2026-10-31 | 完成 M1：配置解析器、协议包层编解码、`chacha20` 8B IV 与 CRC32 对齐、ZFEC 与 `fec_container`；测试从 21 增至 48；`--check-config` 对 4 份样例返回 `No` |
| 2026-10-31 | M2 主体：移植 ikcp（发送/重传/拥塞/窗口探测）、与 C++ 输出逐字节对撞、10% 丢包回环；`KcpUpdater` 最小堆调度与 `PacketCodec`；测试增至 65 |
| 2026-10-31 | 完成 M2：UDP/TCP 封装、`KcpChannel` 端到端流水线（本机 UDP + AES-GCM 实测）；测试增至 71 |
| 2026-10-31 | M3：客户端/服务端握手与 TCP `raw_data` 双向转发；Rust↔Rust 集成测试、Rust 客户端↔C++ 服务端、C++ 客户端↔Rust 服务端均通过；测试增至 73 |
| 2026-10-31 | M3 收尾：新增 `crates/logging`（messages/status/ip 三类文件 + `to_speed_unit`）；握手通道 4 秒回收、会话关闭后 31 秒冲刷、空闲 255 秒强制关闭；服务端保活发送、客户端 30 秒握手超时换端口重试；状态日志每 60 秒输出；测试增至 80 |
| 2026-10-31 | **M3 完成**：1 小时稳定性测试通过（长连接 719/719、新建连接 119/119、0 失败，平均延迟 4.7 ms）；Windows UDP `WSAECONNRESET`（对端端口不可达）降级为 trace |
| 2026-10-31 | **M4 UDP 转发**：服务端按协议分流建立 UDP 会话（双向空闲才回收）；客户端同端口 TCP+UDP 监听、按本地源地址分发独立 KCP 通道；C++ 客户端↔Rust 服务端、Rust 客户端↔C++ 服务端、Rust↔Rust 三向 UDP echo 全部通过；测试增至 81 |
| 2026-10-31 | **M4 FEC**：新增 `transport::fec`（FecSender/FecReceiver），三种包层帧的解析与路由接入客户端/服务端；`fec=10:3` 下 32 KiB 数据与 C++ 双向互通；顺带修复 KCP 大消息缓冲过小的问题；测试增至 87 |
| 2026-10-31 | **M4 mux**：新增 `transport::mux`（`MuxTunnel` / `MuxRecord` / 事件与命令队列）；客户端 `mux_tunnels` 共享通道、TCP/UDP 按 `connection_id` 复用；服务端 mux 会话、目标连接建立、`mux_cancel` 与 4 秒保留期、UDP 空闲回收、`pre_connect_custom_address`；`mux_tunnels=3` 下 3 路并发 TCP + 64 KiB + UDP 与 C++ 双向互通，mux+FEC 组合通过；测试增至 90 |
| 2026-10-31 | docs: 按代码现状校准文档：更新 crate 清单、加密模式数量、代码行数与 FEC/mux 完成状态；补充 `ipv4_only`/`ipv6_only`、客户端目标选择、`blast`、带宽未应用等已知差异 |
| 2026-10-31 | **M4 端口跳跃与默认参数**：服务端监听完整端口范围并共享会话表，按来源自动重绑；客户端 `dport_refresh` 探测与切换 socket、随机目标选择、只监听首个端口；接入 `blast`、带宽 `SetBandwidth` / `ResetWindowValues`、`ipv4_only` / `ipv6_only`；新增通道重绑、blast、带宽、地址族与 `port_hopping_keeps_connection` 测试；测试增至 96；STUN/DDNS 调整为 M6 |
| 2026-10-31 | **C++ 互通验证**：Rust 客户端→C++ 服务端（TCP/UDP）均跨越 2 次跳跃持续收发；C++ 客户端→Rust 服务端 UDP 跨越跳跃持续收发。期间修复服务端握手通道把 `test_connection` 错回为 `keep_alive_response` 的兼容性问题；记录 C++ 端口跳跃仅覆盖 UDP/mux 且由空闲超时触发的语义差异 |
| 2026-10-31 | 版本号显示方式：`--version` 改用 crate 版本（`APP_DISPLAY_VERSION`），与协议兼容目标版本 `APP_VERSION` 区分；提取 `print_version_banner` 统一横幅输出并注明须与 C++ 逐字一致；已知问题登记 #10 |
| 2026-10-31 | 版本号统一：启动横幅、usage 与 `--version` 全部使用根 `Cargo.toml` 的 `version`（`0.1.0`），删除 `APP_VERSION` 常量，程序只对外自报一个版本号；横幅不再与 C++ 版逐字一致，协议兼容目标仍记录在 `docs/dev/migration-plan.md`；已知问题 #10 关闭 |
| 2026-10-31 | docs: 新增使用文档全套（`docs/README.md` 索引、`parameters.md` 参数手册、`client-server.md`、`relay-mode.md`、`fec.md`、`mtu.md`、`custom-ip-mappings.md`、`specific-examples.md`），参照 `legacy-cpp/docs/` 篇目结构、基于 Rust 代码实际行为编写；校准 KCP 预设值（fast1/fast2 `kcp_interval=0`）、`kcp_mtu` 默认值（1418/1405）、带宽后缀与 `/8` 换算怪癖、`{}` 忽略语法、relay 段继承规则等与 C++ 对齐的细节；对 `--try`、relay 运行期、`[custom_input*]` 转发、STUN/DDNS、`fib_*` 等未实现项逐一标注；根 `README.md` 文档导航同步更新 |
| 2026-10-31 | docs: 按代码行为校正文档与注释：`mtu` 优先于 `kcp_mtu`、`listen_on` 仅接受 IP、多配置文件为顺序启动（登记已知问题 #13）、默认仅监听 IPv4（#12）、`ip_address.log` 尚未接入、端口范围含两端；修正 `cipher.rs` 校验字节序注释与过时提示文案；非空行数更新为 9,919 |
| 2026-10-31 | test: 稳定 interop 测试：端口改为进程内统一分配（同一端口同时校验 TCP/UDP、避开常见临时端口段，覆盖 echo/server/client/端口跳跃范围），并让服务端先于客户端启动，消除 mux 启动握手与 UDP 端口尚未监听的竞态；连续 50 次运行 0 失败 |
| 2026-10-31 | fix: mux 通道改为后台维护（`maintain_mux_tunnels`）：建链失败不再让 `Client::run` 退出，而是 2 秒后重试；通道断开后由该任务统一补建，行为对齐 C++ `establish_mux_channels`；无可用隧道时本地 TCP 连接排队等待（最长 30 秒）；新增 `interop.rs::mux_client_queues_until_server_starts` 回归测试；测试增至 97 |
| 2026-11-01 | **fix（跨设备连不上）**：`PacketCodec::open_frame` 的时间窗校验误写成单向无符号减法，只要对端时钟比本机快 1 秒，差值就回绕成约 40 亿而被判为过期，**两台设备（时钟未严格同步）完全无法握手**。改为与 C++ `calculate_difference` 一致的 `abs(now - ts) > 255`，并补回归测试（±1、±120 秒接受，±300 秒丢弃） |
| 2026-11-01 | fix: 握手阶段忽略底层 recv 错误（ICMP 端口不可达导致的 `WSAECONNRESET` / `ConnectionRefused`），对齐 C++ `udp_client::handle_receive` 的「出错即重新挂起接收」；此前客户端会立即放弃本地连接，服务端晚启动或防火墙回 ICMP 时表现为「连不上」 |
| 2026-11-01 | fix: 域名解析保留**全部**候选地址（`resolve_endpoints_filtered`），握手与端口跳跃逐个尝试；此前只取 `lookup_host` 的第一个结果，DDNS/`localhost` 解析到 IPv6 而服务端只监听 IPv4 时会一直超时 |
| 2026-11-01 | fix: 未指定 `ipv4_only` / `ipv6_only` 时监听地址改为 `[::]` 双栈（与 C++ `udp_server::initialise` 一致，仅 `ipv6_only` 才置 `IPV6_V6ONLY`），修复默认无法被 IPv6 / `localhost(::1)` 访问的问题；已知问题 #12 关闭 |
| 2026-11-01 | chore: 发布产物体积优化 —— release 打开 `strip`/fat LTO、`opt-level = "s"`（加密/KCP/转发/校验和热点仍为 `-O3`）、`tokio` 由 `full` 收窄到实际特性、`clap` 去掉彩色与拼写建议、`tracing-subscriber` 用 `Targets` 取代 `env-filter`（省掉 regex）；Windows GNU 产物体积 2.78 MB → 1.26 MB（约 -55%），musl 静态产物 2.41 MB → 1.44 MB（约 -40%） |
| 2026-11-01 | **M4 完成（自定义映射）**：客户端在 `ignore_listen_*` 下按 `[custom_input*]` 逐条监听并随 `request_initialise_packet_with_address` 下发目标；mux 模式发 `pre_connect_custom_address`；服务端 `ignore_destination_*` 生效，mux 握手不再要求 `destination_*`。新增 4 个自定义映射 interop 测试、100 并发 mux TCP 验收测试、进程内丢包代理下的 FEC 端到端测试，以及 3 个客户端监听项单元测试；测试增至 106，已知问题 #7 关闭 |
| 2026-11-01 | refactor: 拆分三个 1000+ 行源文件（纯移动、行为不变）——`kcp/src/lib.rs` 1354 → 204 行，拆为 `api`/`send`/`recv`/`ack`/`timer`/`tests`；`client/src/lib.rs` 1541 → 212 行，拆为 `listen`/`state`/`handshake`/`tcp`/`udp`/`mux`/`port_hop`/`util`；`server/src/lib.rs` 1300 → 145 行，拆为 `state`/`socket`/`handshake`/`data`/`relay`/`mux`/`status`/`util`。所有单元测试、C++ 对撞向量与 13 个 interop 测试保持通过 |
| 2026-11-01 | docs: 新增 [`refactor-backlog.md`](refactor-backlog.md)：记录跨模块重复审计（完全重复 5 类、结构性重复 3 类、未接入/死 API 清单）、与 C++ 原版逐项对照，以及阶段 1–5 的优化路线与决策（`KcpUpdater` 接入、config→kcp 依赖、`WaitQueue` 保留） |
| 2026-11-01 | docs: 新增 [`relay-plan.md`](relay-plan.md)：拆解 C++ `relay.cpp` 的握手/数据/清理/端口跳跃行为，给出 Rust 侧模块划分、任务模型、与 C++ 的对齐清单、R0–R5 分步实施与测试计划，并登记 C++ 上游的 4 处复制粘贴缺陷（Rust 侧有意偏离） |
| 2026-11-01 | refactor: 完成 [`refactor-backlog.md`](refactor-backlog.md) 阶段 1（公共层收口，等价重构）——`is_connection_reset`/`wildcard_addr`/`KcpChannel::{set_rx_minrto, apply_handshake_tuning, apply_data_tuning}` 下沉到 transport；`UserSettings::{kcp_config, fec_parameters}`、`KcpConfig::handshake` 下沉到 config/kcp；`log_warning`、`StatusSummary` + `print_status_summary` 下沉到 logging；删除 client/server `util.rs` 中的重复实现，5 处通道初始化改调助手；行为、线格式与状态日志文本不变；测试增至 115 |
| 2026-11-01 | refactor: 阶段 5 relay 前置（前三项，等价重构）——新增 `transport::PortHop`（端口探测与重绑，client 只保留定时/选目标/日志）、`transport::KeepAlive`（`new` 用于 select 循环、`tick_at` 用于服务端维护任务，统一 client TCP/UDP、mux、server 四处）、`transport::open_handshake_channel`（client 握手与端口探测共用）；relay 的空闲清理规则与 server 不同，留到 relay R2 随维护任务落地；测试增至 118 |
| 2026-11-01 | **feat: relay R1**：新增 `crates/relay` 的 `listener` / `handshake` / `session` / `state` / `util` —— ingress UDP 监听与 conv 分派、两段 conv 0 通道之间的 `initialise` 中继（带宽与端口范围按段夹取、原地改写 26 字节头）、每会话任务双向转发（raw_data / mux_* / keep_alive 控制帧）；`protocol` 新增 `SettingsWrapper::modify_details`；`main` 改为把根配置传给 `Relay::new`；新增 `relay_tcp_roundtrip` / `relay_udp_roundtrip`（两段使用不同加密算法与 KCP 预设），测试增至 126；已知问题 #1 拆分并登记 relay 的 `listen_on` 不复刻 v4-mapped 映射 |
| 2026-11-01 | **feat: relay R2**：新增 `relay/maintenance`（每秒空闲判定：任一方向 > 255 秒，或转发/两方向同时超过 `[forwarder].udp_timeout`；两侧按各自 `keep_alive` 独立发送保活）与 `relay/status`（两段式摘要 `[Client <-> This]` / `[This <-> Remote]`，含 raw/inner 四项速率、fec recover、每 conv 两侧 SRTT）；修正 C++ 把 ingress FEC 恢复数记到 forwarder 的问题；`state` 恢复协议类型、`last_data_transfer` 与两段计数器；新增 `relay_keep_alive_stays_connected` 与空闲判定/摘要格式单元测试，测试增至 131 |
| 2026-11-01 | **feat: relay R3**：`transport` 下沉 `drain_old_socket` 供 client/relay 共用；新增 `relay/port_hop` —— 每次成功转发后按 `[forwarder].dport_refresh` 检查并后台探测，成功后立即 rebind 并替换 egress socket，旧 socket 继续收 2 秒；`RelaySession` 的 `egress_socket` 改为可替换并新增 `hopping_at` / `hopping_testing`；与 C++ 的两处有意偏离（失败重试、一步切换）见 `relay-plan.md`；新增 `relay_port_hopping_keeps_connection`，测试增至 133 |
| 2026-11-01 | **test: relay R4**：`start_relay_pair` 增加 `RelayPairOptions`（`mux_tunnels` / `fec`），新增 4 个端到端用例——mux 双隧道 TCP+UDP、20 并发 mux TCP、两段 FEC 32 KiB、客户端 `[custom_input]` + 服务端 `ignore_destination_*`（验证 `user_input` 目标经 relay 原样透传）；relay 本体无需改动，测试增至 137 |
| 2026-11-01 | **feat: R5 `--try`**：新增 `crates/tester`（`kcptube-tester`）——对「每个目标地址 × 每个端口」并发建立 conv 0 通道、发送 `test_connection`、30 秒超时；client 取根配置、relay 取 `[forwarder]` 段；输出与 C++ `PrintResults` 逐字一致；`main` 的 `--try` 分支接入；与 C++ 的差异：地址解析失败记为全部失败并继续（C++ 中止整个测试）；新增 3 个单元测试与 2 个集成测试，已知问题 #2 关闭，测试增至 142 |
| 2026-11-01 | docs: 使用文档与开发者文档分开放——`docs/` 仅保留面向使用者的文档（`README.md` 首页、`parameters.md`、`client-server.md`、`relay-mode.md`、`fec.md`、`mtu.md`、`custom-ip-mappings.md`、`specific-examples.md`）；本文件与 `migration-plan.md` / `refactor-backlog.md` / `relay-plan.md` 移入 `docs/dev/`，新增 [`docs/dev/README.md`](README.md) 开发文档索引；同步更新根 `README.md`、`AGENTS.md`、`docs/README.md`、`docs/client-server.md` 及源码注释中的路径引用，全部相对链接校验通过 |
| 2026-11-01 | **fix（relay 代码验证收尾）**：① 保活——`RelaySession` 建会话时把两侧 `next_keep_alive` 初始化为当前时刻；此前一直为 `i64::MAX`，`send_keep_alive_if_due` 永远提前返回，relay 实际从不主动保活。新增 `scheduled_keep_alive_is_actually_sent`。② `disconnect` / `failure`——改为先记 `now + KEEPALIVE_TIMEOUT`（31 秒）再关闭，会话任务再保留 4 秒冲刷，对齐 C++ `expiring_kcp`；新增 `disconnect_expiry_waits_for_scheduled_time`。③ 目标解析失败或无法建立出口通道时向客户端回 `inform_error_packet`，不再干等 30 秒握手超时；新增 `relay_unreachable_target_reports_failure`。④ 新增 `relay_client_port_hopping_keeps_connection`（4 个 relay 端口，读取客户端日志确认确实跳跃）与 `initialise` 端口范围/带宽改写单测；修正 `relay-plan.md` §2.3/§2.5/§7.1 与过时模块注释。测试增至 149 |
| 2026-11-01 | **T4 / T10 验收完成**：`scripts/verify-t10-relay-interop.ps1` 用桌面 C++ `20260329` 原版跑通 `core`（C++ client → Rust relay → C++ server）、`fec`、`mux` 与 `reverse`（Rust client → C++ relay → Rust server）；`scripts/verify-t4-relay-keepalive.ps1` 在客户端 `keep_alive=0`、relay `keep_alive=1` 下空闲 **300 秒**后 TCP/UDP 仍通，已知问题 #1、#14 关闭。新增 `scripts/build-legacy-mingw.ps1`、`net_echo.py`、`net_probe.py` 与两份验收脚本。同步登记 C++ relay 的两个限制（`[listener]` 段 `listen_on` 崩溃、`relay.cpp:1369` egress 误用 ingress 加密参数） |
| 2026-11-01 | docs: 按代码现状校准文档瑕疵——`docs/README.md` 的模式列表与状态表统一为「中继模式可用」；`relay-mode.md` 状态行更新为已完成 C++ T10 四个场景的互通验收；`migration-plan.md` crate 数改为 12（11 lib + 1 bin）、勾选 M5 的 relay/tester 与 Linux 构建、把「静态链接」拆为未完成项、补充端到端矩阵的口径说明；`refactor-backlog.md` 阶段 5 的空闲清理按 R2 的实际落地更新；`relay-plan.md` 统一 `relay.cpp` 行数口径并修正 relay 保活的实际实现（自管 `next_keep_alive`，未复用 `transport::KeepAlive`）；根 `README.md` 目录结构补 `tester/`；`progress.md` 代码量更新为 55 个源文件 / 14,114 行（测试仍为 149，`cargo test --workspace` 实跑复核） |

---

## 附录：C++ 参考版编译（MinGW）

```bash
# 依赖
pacman -S mingw-w64-x86_64-gcc mingw-w64-x86_64-cmake \
          mingw-w64-x86_64-ninja mingw-w64-x86_64-asio \
          mingw-w64-x86_64-libbotan

# 配置（注意两处 MinGW 适配）
cmake -S legacy-cpp -B build/legacy -G Ninja \
      -DCMAKE_CXX_FLAGS="-ID:/msys64/mingw64/include/botan-3" \
      -DCMAKE_CXX_STANDARD_LIBRARIES=-ldnsapi
cmake --build build/legacy -j
```

已知需要的 3 处源码适配（仅 MinGW，未合入 `legacy-cpp/`，保持其与上游一致）：

1. `dns_helper.cpp`：`#include <windnsdef.h>` 在 MinGW 下不存在（`windns.h` 已包含定义）
2. `dns_helper.cpp`：MinGW 的 `DNS_TXT_DATAW::pStringArray` 是 `PSTR` 而非 `PWSTR`
3. 链接需要显式 `-ldnsapi`（MinGW 忽略 `#pragma comment(lib)`）

> 若后续需要频繁做互通测试，建议在 `legacy-cpp/` 之外维护一个 `scripts/build-legacy-mingw.ps1`，通过 `-D` 与补丁副本完成上述适配，避免污染参考实现。
