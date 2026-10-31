# kcptube Rust 移植计划

> 状态：v1 草案（2026-10-31）
> 适用分支：`rs-dev`
> 参考实现：`legacy-cpp/`（C++ 版，`app_version = 20260329`）

---

## 1. 背景与目标

### 1.1 现状

- 原项目 KCP Tube 是 C++20 实现，网络层基于 asio，加密/FEC 依赖 Botan3，共约 **14.3k 行**（含第三方 ikcp 1.1k 行）。
- 本仓库将其完整保留在 `legacy-cpp/`，作为协议参考与互通测试基准。
- Rust 侧已建立 workspace 骨架（10 个 crate + 1 个可执行程序），详见 `docs/progress.md`。

### 1.2 目标

1. **线格式 1:1 兼容**：Rust 版可以与现有 C++ 版客户端/服务端混跑，配置文件名与语义保持一致。
2. **行为等价**：KCP 参数预设、重传/拥塞行为、端口跳跃、mux 语义与 C++ 版一致。
3. **可验证**：每个协议模块都有来自 C++ 参考实现的测试向量，关键路径有互通测试。
4. **可维护**：模块边界清晰，纯逻辑与 IO 分离，尽量不使用 `unsafe`。

### 1.3 非目标（本计划内不做）

- 新协议、新加密算法、新配置语法
- 图形界面、Web 面板
- macOS 官方支持（可编译但不保证，见 M5）
- 以性能为唯一目标的重构（先对齐，再优化）

### 1.4 成功标准

- [ ] Rust 客户端 ↔ C++ 服务端：TCP 转发可用，单连接吞吐不低于 C++ 版的 80%
- [ ] Rust 服务端 ↔ C++ 客户端：同上
- [ ] `mode=client / server / relay` 三种模式均可运行
- [ ] 5 种加密模式（含 `xor`、`none`）全部与 C++ 互通
- [ ] FEC 开启/关闭均可互通
- [ ] mux_tunnels > 1 时多路复用正常
- [ ] 端口跳跃（`dport_refresh`）可用
- [ ] 配置错误提示与 C++ 版行为一致（`--check-config`）

---

## 2. 总体策略

| 原则 | 说明 |
|---|---|
| **线格式优先** | 先保证「能互通」，再考虑代码优雅。任何简化都不能改变字节流。 |
| **向量驱动** | 每个模块开工前，先用 `legacy-cpp/` 生成固定输入/输出向量，Rust 侧写成 `#[test]`。 |
| **分层解耦** | `protocol` / `crypto` / `fec` 为零 IO 纯逻辑 crate，可独立测试。 |
| **渐进交付** | 先客户端后服务端，先 TCP 后 UDP，先单通道后 mux，先固定端口后端口跳跃。 |
| **参考实现只读** | `legacy-cpp/` 不做功能修改；如需 MinGW 编译适配，只改构建脚本的独立副本。 |
| **可回退** | 每完成一个里程碑打 tag，互通失败时可快速定位到某个阶段。 |

---

## 3. 源码盘点与映射

### 3.1 C++ 侧

| 文件 | 行数 | 职责 | Rust 目标 crate | 难度 | 阶段 |
|---|---:|---|---|---|---|
| `modes/client.cpp` | 1917 | 客户端主逻辑：监听、握手、端口跳跃、状态日志 | `kcptube-client` | 高 | M3/M4 |
| `modes/relay.cpp` | 1802 | 中继：ingress/egress 双段 | `kcptube-relay` | 高 | M5 |
| `modes/server.cpp` | 1323 | 服务端：握手、转发、STUN、DDNS | `kcptube-server` | 高 | M3/M4 |
| `networks/connections.cpp` | 1294 | asio 会话封装、包编解码、字节序 | `kcptube-protocol` + `kcptube-transport` | 中 | M1/M2 |
| `shares/configurations.cpp` | 1010 | `.conf` 解析、参数校验、预设展开 | `kcptube-config` | 中 | M1 |
| `networks/mux_tunnel.cpp` | 660 | mux 多路复用、缓存队列 | `kcptube-transport` | 高 | M4 |
| `shares/data_operations.cpp` | 642 | 加密调度、FEC 容器、xor 差分 | `kcptube-crypto` + `kcptube-fec` | 中 | M1 |
| `networks/connections.hpp` | 621 | 会话类定义、包结构体 | 同上 | 中 | M1/M2 |
| `shares/aead.hpp` | 471 | 4 种 AEAD 实现、IV 派生 | `kcptube-crypto` | 中 | M1 |
| `modes/tester.cpp` | 392 | `--try` 连通性测试 | `kcptube-tester`（待建） | 中 | M5 |
| `networks/kcp.cpp` | 292 | ikcp 的 C++ 包装 | `kcptube-kcp` | 低 | M2 |
| `networks/dns_helper.cpp` | 288 | DNS TXT 查询（Windows DnsQuery / resolv） | `kcptube-transport` | 中 | M4 |
| `shares/share_defines.cpp` | 237 | 参数工具、日志、ZFEC 初始化 | `kcptube-config` / `kcptube-fec` | 低 | M1 |
| `networks/stun.cpp` | 118 | STUN 打洞（RFC3489/8489） | `kcptube-transport` | 中 | M4 |
| `networks/kcp_updater.cpp` | 117 | KCP 定时驱动线程 | `kcptube-transport` | 中 | M2 |
| `3rd_party/ikcp.cpp` | 1109 | KCP 协议（cnbatch 改版） | `kcptube-kcp` | 中 | M2 |
| `3rd_party/task_thread_pool.hpp` | 433 | 线程池 | 由 tokio 取代 | 低 | M2 |

### 3.2 已建立的 Rust crate

```text
crates/
  protocol/    常量、Feature/ProtocolType、包层头、字节序工具   （零依赖）
  crypto/      SHA3 派生、IV 展开、AEAD、xor 差分
  fec/         FEC 配置 + ZFEC 接口
  kcp/         KCP 配置 + 接口
  config/      UserSettings（字段已对齐）+ 解析器（待实现）
  transport/   tokio 会话与 KCP 调度（接口）
  client/      客户端模式（仅入口）
  server/      服务端模式（仅入口）
  relay/       中继模式（仅入口）
  kcptube/     可执行程序（clap）
```

规划中的新增 crate：

- `kcptube-tester`：`--try` 连通性测试（对应 `tester.cpp`）
- `kcptube-platform`（可选）：Windows `DnsQuery_W`、FreeBSD `SO_SETFIB`、BSD `resolv` 的隔离封装

---

## 4. 目标架构

### 4.1 crate 依赖图

```text
                 ┌──────────────┐
                 │  kcptube     │  (bin)
                 └──────┬───────┘
        ┌───────────────┼────────────────┐
        ▼               ▼                ▼
  kcptube-client  kcptube-server   kcptube-relay
        └───────────────┼────────────────┘
                        ▼
                kcptube-transport
                 ┌──────┴───────┐
                 ▼              ▼
           kcptube-kcp     kcptube-protocol
                 │              │
                 └──────┬───────┘
                        ▼
             kcptube-crypto   kcptube-fec
                        │
                        ▼
                 kcptube-config
```

### 4.2 运行时模型

| C++ | Rust |
|---|---|
| `asio::io_context ioc_light`（1 线程） | 单线程 task 组（定时器、握手调度） |
| `asio::io_context ioc_heavy`（N 线程） | 多线程 `tokio` runtime（数据面） |
| `asio::steady_timer` | `tokio::time::sleep_until` |
| `KCPUpdater` 独立线程 + 条件变量 | 单独 task + `BinaryHeap` 最小堆 |
| `std::shared_ptr<session>` 生命周期 | `Arc<Session>` + `CancellationToken` |
| `asio::post` 跨线程 | `mpsc` / `JoinSet` |
| 线程池 `task_thread_pool` | 数据面直接由 tokio 调度，移除线程池 |

### 4.3 状态与并发约定

- 每个 TCP 会话一个 task；UDP 监听器一个 task，收到数据后按对端分发。
- KCP 会话由 `Mutex<Kcp>` 保护，只在调度 task 内驱动（避免锁竞争）。
- 连接表使用 `dashmap` 或 `Mutex<HashMap>`，键与 C++ 版一致：
  - `(conv << 32) + connection_id`
  - UDP 对端 `SocketAddr` → mux 记录
- 所有超时清理逻辑照搬 C++ 的常量（`gbv_*`，见 `crates/protocol/src/constant.rs`）。

---

## 5. 协议关键事实（移植必读）

> 本节来自对 `legacy-cpp/src` 的逆向，目的是让后续开发不必反复翻 C++ 代码。

### 5.1 分层

```text
UDP 数据报
 └─ 加密层：AEAD 密文 || iv_raw(2 字节)
      └─ 包层 packet_layer: timestamp(u32 LE) + data
           ├─ FEC 冗余包 packet_layer_fec: + sn(u32 BE) + sub_sn(u8) + kcp_conv(u32 BE)
           ├─ FEC 原始包 packet_layer_data: + sn(u32 BE) + sub_sn(u8)
           └─ 数据层 data_layer: 1 字节（低 4 位 feature，高 4 位 protocol）
                └─ mux 包装 mux_data_wrapper: connection_id(u32 BE)
```

### 5.2 字节序（最容易出错）

| 字段 | 字节序 | C++ 写法 |
|---|---|---|
| `timestamp` | **小端** | `host_to_little_endian` |
| `sn` | **大端** | `htonl` |
| `kcp_conv` | **大端** | `htonl` |
| `connection_id` | **大端** | `htonl` |
| `uid` | **大端** | `htonll` |
| 端口 / 单字节 | 原样 | — |

KCP 本体（ikcp）是标准大端格式。

### 5.3 时间戳

- 秒级 Unix 时间，取低 32 位。
- 接收端与本地时间差 > **255 秒** 直接丢弃（`gbv_time_gap_seconds`）。
- 保活超时 = 255 >> 3 = **31 秒**。

### 5.4 加密

- key = `SHA3-256(password)`。
- IV 派生（`head_tail_xor` = 前后两半异或）：
  - `aes-gcm`：iv 16B
  - `aes-ocb`：iv 12B（SHA3-384 反复折叠）
  - `chacha20`：iv **8B**（SHA3-256 反复折叠）
  - `xchacha20`：iv 24B（SHA3-384 折叠一次）
- 每个包发送前用 2 字节随机数展开重写 IV，并把该 2 字节附加在密文尾部。
- AAD 固定为 `"KCP PortHopping"`。
- `xor` / `none`：数据尾部附 CRC32 折叠的 2 字节校验；`xor` 额外做一次前向差分。
- **已验证**：5 种模式（`aes-gcm` / `aes-ocb` / `chacha20` / `xchacha20` / `xor`）均与 Botan 3.13 逐字节一致。
- `chacha20` 使用 **8 字节 nonce**，Botan 会走**原始版** ChaCha20-Poly1305（draft-agl）：
  `AAD || LE64(AAD 长度) || 密文 || LE64(密文长度)`，不做 16 字节补齐；
  12/24 字节 nonce 才走 RFC 8439 的补齐 + 两个 LE64 长度。
- `xor` / `none` 的 2 字节校验：Botan `CRC32::final_result` 用 `store_be` 输出，
  因此线上字节是大端 `(crc >> 16) ^ (crc & 0xffff)`（早期误用小端，已修正）。

### 5.5 FEC

- 使用 Botan 内置 ZFEC（Luigi Rizzo 的 Vandermonde 方案，与 tahoe-lafs/zfec 兼容），参数 `fec=k,r`。
- 编码矩阵：前 k 行是单位阵，第 k..n 行由 `create_inverted_vdm` 与 Vandermonde 行相乘得到。
- 发送端攒够 k 个包后拼接编码，只发送冗余 share（下标 ≥ k）。
- 冗余包带 `packet_layer_fec` 头；原始包带 `packet_layer_data` 头。
- 每个 share 带 4 字节大端 share id。
- 接收端按 `(sn, sub_sn)` 重组，凑满 k 个即可恢复。
- Rust 实现已与 Botan `ZFEC(4,6)` 做过 share 逐字节对撞（见 `crates/fec/src/lib.rs`）。

### 5.6 握手

1. 客户端向服务端 UDP 端口发送 `initialise` 包（内含 `settings_wrapper`：uid、端口范围、带宽、目标地址）。
2. 服务端校验后返回 `initialise` 响应，客户端确认后建立 KCP 数据通道。
3. 之后所有业务数据走 `raw_data`（单通道）或 `mux_transfer`（多路复用）。
4. 保活用 `keep_alive` / `keep_alive_response`。

### 5.7 端口跳跃

- 由 `dport_refresh` 触发，先发 `test_connection` 探测新端口是否可用。
- 可用则切换发送 socket（旧 socket 延迟回收），并把端点替换为原子操作。
- 服务端需同时监听端口范围（`listen_port=3000-3010`）。

### 5.8 配置语法要点

- 每行 `key=value`，`#` 开头为注释，`name` 与 `value` **统一转小写**后比较。
- 因此 `encryption_algorithm=AES-GCM` 与 `aes-gcm` 等价。
- 端口支持范围与列表：`3000-3010`、`3000,3001,3005`。
- relay 模式使用分段：`[listener]`（ingress）与 `[forwarder]`（egress）。
- 参数表见 `legacy-cpp/docs/parameters_zh-hans.md`。

---

## 6. 阶段计划

> 人日为单人估算，含测试与文档，不含返工缓冲（总缓冲约 15%）。

### M0 基线搭建（已完成 ✅）

任务：

- [x] 环境确认（Rust 1.96 / GCC 16.2 / asio / Botan3）
- [x] C++ 参考版编译成功并可运行
- [x] 迁移 C++ 代码至 `legacy-cpp/`
- [x] 建立 Rust workspace（10 crate）
- [x] 加密层与 Botan 向量对撞（3 种模式一致）
- [x] CI（fmt / clippy / test，ubuntu + windows）


验收：`cargo test --workspace` 全绿，`legacy-cpp/` 可独立编译。

---

### M1 协议基础层（已完成 ✅）

目标：把「纯字节」的部分全部做完并验证，之后不再碰协议细节。

任务：

- [x] **protocol**：`packet_layer` / `packet_layer_data` / `packet_layer_fec` / `data_layer` / `mux_data_wrapper` 的编码与解码（使用 `ByteOrder` 约定）
- [x] **protocol**：`settings_wrapper` 与 `pre_connect_custom_address` 的变长编解码
- [x] **protocol**：`--try` 使用的测试包构造
- [x] **crypto**：`chacha20`（8B IV）与 Botan 的 tag 对齐（原始版 draft-agl AEAD）
- [x] **crypto**：`xor` / `none` 的 CRC32 校验与 C++ 逐字节对齐
- [x] **crypto**：加密帧整体（密文 + 2 字节 iv_raw）与 C++ 对齐
- [x] **fec**：ZFEC 实现（GF(2^8) + Vandermonde），与 Botan 的 share 逐字节对齐
- [x] **config**：完整解析器（参数、范围、列表、映射、`[listener]`/`[forwarder]`、kcp 预设展开）
- [x] **config**：错误信息与 C++ 版一致
- [x] 测试：`service/config_sample_*.conf` 全部可解析，并与 C++ 版解析结果对照

验收（DoD）：✅ 全部满足

- 每个模块有 ≥ 5 个向量测试（config 15、crypto 12、protocol 11、fec 7）
- 加密/FEC 已用 Botan 生成固定向量逐字节比对
- `kcptube --check-config service/config_sample_*.conf` 输出 `Error Found in Configuration File(s): No`

---

### M2 KCP 与传输层（已完成 ✅）

目标：KCP 能在 Rust 侧正确收发并定时驱动。

任务：

- [x] **kcp**：移植 `ikcp.cpp`（send/input/recv/update/flush/check）
- [x] **kcp**：`SetBandwidth`、`ResetWindowValues`、`WaitQueue*` 等改版特性
- [x] **kcp**：与 C++ 版做「同一输入序列 → 同一输出包序列」测试
- [x] **transport**：`KcpUpdater`（最小堆，语义对齐 C++ 线程版）
- [x] **transport**：TCP 会话封装（读、写、背压、`into_split`）
- [x] **transport**：UDP 监听/发送封装（`socket2`、`SO_REUSEADDR`、双栈；`SO_SETFIB` 留到 M5）
- [x] **transport**：数据包发送/接收流水线（加密 → 包层 → KCP → UDP，见 `KcpChannel`）
- [x] 测试：回环（loopback）下 10% 丢包仍能完整传输

验收：

- [x] 进程内两个 KCP 端点互通，丢包 10% 时仍能正确传输
- [x] 定时器精度与 C++ 版一致（KCP `interval` 生效）

> FEC / mux 接入流水线属于 M4；M3 先跑「关闭 FEC、单通道」的场景。

---

### M3 最小互通：TCP 单通道（进行中，约 90%）

目标：**Rust 客户端 ↔ C++ 服务端** 的 TCP 转发跑通，这是第一个可交付里程碑。

任务：

- [x] **server**：UDP 监听、`initialise` 握手、`raw_data` 转发到目标 TCP
- [x] **client**：本地 TCP 监听、握手、数据转发
- [x] **client/server**：KCP 通道建立与断开、保活
- [x] 加密全流程（默认 `aes-gcm`）
- [ ] 日志：关键事件与 C++ 版对齐（status/messages/ip）
- [x] 互通测试：C++ 服务端 + Rust 客户端、Rust 服务端 + C++ 客户端、Rust↔Rust 自动化

验收：

- [x] 双向混跑（Rust client ↔ C++ server 和 C++ client ↔ Rust server）均可传输
- [ ] 连续 1 小时无断流
- [x] 断开重连正常（每次 TCP 连接独立握手）

---

### M4 完整客户端/服务端（约 14~18 人日）

目标：把「能用」变成「和 C++ 版一样好用」。

任务：

- [ ] **UDP 转发**（`udp_timeout`、UDP 映射表）
- [ ] **mux**：`mux_tunnels`、`mux_transfer` / `mux_cancel` / `pre_connect_custom_address`
- [ ] **mux**：缓存队列与背压（对应 `mux_tcp_cache` / `mux_udp_cache`）
- [ ] **端口跳跃**：`dport_refresh`、`test_connection`、socket 切换、旧 forwarder 延迟回收
- [ ] **STUN**：服务端打洞，IPv4/IPv6
- [ ] **DDNS**：外部脚本回调（`update_ipv4` / `update_ipv6`）、`destination_dnstxt`
- [ ] **带宽限制**：`inbound_bandwidth` / `outbound_bandwidth`
- [ ] **状态日志**：`log_path` 下的 status / messages / ip 三个文件，格式对齐
- [ ] **自定义 IP 映射**：`->` 语法
- [ ] `blast`、`udp_timeout` 等剩余参数

验收：

- mux 下 100 并发 TCP 连接稳定
- 端口跳跃切换期间连接不断
- STUN 成功获取公网地址并写入回调脚本

---

### M5 relay / tester / 跨平台 / 性能（约 9~13 人日）

任务：

- [ ] **relay**：`[listener]` / `[forwarder]` 双段配置与转发
- [ ] **tester**：`--try` 连通性测试（延时、丢包统计与输出格式）
- [ ] **平台**：Windows `DnsQuery_W`（`--features windows-dns`）
- [ ] **平台**：FreeBSD `SO_SETFIB`、`resolv`（DNS TXT）
- [ ] **平台**：Linux 构建与静态链接
- [ ] **性能**：基准测试（吞吐、P99 延迟、内存），与 C++ 版对比
- [ ] **性能**：必要优化（零拷贝、批量收发、`SO_REUSEPORT`）
- [ ] **发布**：交叉编译脚本、GitHub Release、Docker 镜像

验收：

- 三种模式全部可用
- 性能达到 C++ 版的 80% 以上（延迟不劣化）
- 至少 Windows + Linux 双平台 CI 绿

---

### 工作量汇总

| 阶段 | 人日 | 累计 |
|---|---:|---:|
| M0 基线 | 4（已完成） | 4 |
| M1 协议基础 | 8~11 | 12~15 |
| M2 KCP/传输 | 10~14 | 22~29 |
| M3 TCP 互通 | 7~10 | 29~39 |
| M4 完整模式 | 14~18 | 43~57 |
| M5 relay/跨平台 | 9~13 | 52~70 |

关键路径：M1 → M2 → M3。M4 内部可并行（UDP、mux、端口跳跃由不同人认领）。

---

## 7. 兼容性验证方案

### 7.1 三级验证

| 级别 | 方法 | 频率 |
|---|---|---|
| L1 单元向量 | C++ 生成固定输入/输出，Rust 单测比对 | 每次提交 |
| L2 组件对撞 | Rust 加密/FEC 输出喂给 C++ 解码，反向亦然 | 每阶段 |
| L3 端到端互通 | 4 种组合（Rust/C++ × client/server）跑真实流量 | 每个里程碑 |

### 7.2 向量生成方法

```bash
# 在 legacy-cpp 下编译出参考程序
cmake -S legacy-cpp -B build/legacy -G Ninja && cmake --build build/legacy

# 生成向量（示例：加密）
# 同一 key/password/plain，分别输出 hex，写入 crates/crypto/tests/vectors.rs
```

约定：向量文件放在各 crate 的 `tests/` 或 `#[cfg(test)]` 模块内，注明生成时间与 C++ 版本。

### 7.3 端到端测试矩阵

| # | 客户端 | 服务端 | 协议 | 加密 | FEC | mux | 端口跳跃 |
|---|---|---|---|---|---|---|---|
| 1 | Rust | C++ | TCP | aes-gcm | off | 1 | off |
| 2 | C++ | Rust | TCP | aes-gcm | off | 1 | off |
| 3 | Rust | C++ | UDP | aes-ocb | on | 1 | off |
| 4 | Rust | C++ | TCP | xchacha20 | on | 4 | on |
| 5 | C++ | Rust | TCP+UDP | chacha20 | on | 4 | on |
| 6 | Rust | Rust | 全部 | 全部 | on | 8 | on |

### 7.4 排查工具

- 两侧都提供 `RUST_LOG=debug` / C++ 日志开关，输出包头关键字段（timestamp/sn/sub_sn/conv）。
- 统一抓包格式：`tcpdump -w` 后用脚本解析 UDP 负载的 2 字节 iv_raw 位置。

---

## 8. 风险登记表

| # | 风险 | 影响 | 概率 | 应对 |
|---|---|---|---|---|
| R1 | `chacha20` 8B IV 的 Poly1305 tag 与 Botan 不一致 | 该加密模式无法互通 | ✅ 已解决 | Botan 对 8B nonce 使用原始版 AEAD 编码 |
| R2 | ZFEC share 格式与 Botan 不同 | FEC 开关下数据错乱 | ✅ 已解决 | 按 Botan `zfec.cpp` 移植并做 share 对撞 |
| R3 | mux 状态机还原不完整 | 多路复用下偶发卡死/泄漏 | 中 | 先单通道；引入重连与超时清理的单元测试；逐条对照 C++ |
| R4 | 端口跳跃 socket 切换语义 | 切换瞬间丢包/连接中断 | 中 | 复用 C++ 的延迟回收策略；只在 M4 做，不阻塞 M3 |
| R5 | 无协议文档，理解偏差 | 隐性不兼容，难定位 | 高 | 本计划第 5 节沉淀协议事实；每个新发现立即补文档 |
| R6 | Windows 特有 API（DnsQuery） | DDNS 功能缺失 | 低 | 用 `windows` crate 或直接调用 `Dnsapi` |
| R7 | 上游协议演进 | 无法跟随升级 | 中 | 锁定 `20260329`；每次上游发版做 diff 并记录到进度文档 |
| R8 | 吞吐不达标 | 用户不愿迁移 | 中 | M5 做基准与优化；必要时用 `mio`/`io_uring` 直连 |
| R9 | 无自动化回归 | 静默错误 | 高 | CI 强制 test；L2/L3 测试纳入里程碑验收 |

---

## 9. 工程规范

### 9.1 代码

- `unsafe` 默认禁止；确需使用时必须附安全说明与测试。
- 错误统一 `thiserror`，对外 `anyhow`；禁止 `unwrap()` 出现在非测试代码。
- 纯逻辑 crate（protocol/crypto/fec）不得依赖 tokio。
- 所有公开项写文档注释（中文），协议相关注明对应的 C++ 文件与行号。

### 9.2 提交与分支

- 分支：`rs-dev` 开发，`main` 保持与上游可对比状态。
- 提交信息：`<类型>: <中文描述>`，类型用 `feat / fix / refactor / test / docs / chore`。
- 每个里程碑结束打 tag：`m1-protocol`、`m2-kcp` 等。

### 9.3 质量门禁

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI（`.github/workflows/rust.yml`）在 ubuntu 与 windows 上执行上述命令。

### 9.4 文档

- 每完成一个任务，更新 `docs/progress.md` 的状态表与变更记录。
- 协议相关的新发现必须先写进本文档第 5 节，再改代码。

---

## 10. 文档维护

| 文档 | 内容 | 更新时机 |
|---|---|---|
| `docs/migration-plan.md`（本文） | 目标、架构、阶段、协议事实 | 范围或协议理解变化时 |
| `docs/progress.md` | 当前状态、测试数、已知问题 | 每次提交/里程碑 |
| `README.md` | 对外说明 | 里程碑发布时 |
| `legacy-cpp/docs/` | 上游使用文档（只读） | 不修改 |

---

## 附录 A：常用命令

```bash
# 构建与测试
cargo build --workspace
cargo test --workspace
cargo run -p kcptube -- --help

# 编译 C++ 参考实现（MinGW，需先装 msys2 依赖）
cmake -S legacy-cpp -B build/legacy -G Ninja
cmake --build build/legacy -j

# 配置检查（Rust 版）
cargo run -p kcptube -- --check-config service/config_sample_client.conf
```

## 附录 B：里程碑验收清单

- [x] M1：所有协议向量测试通过，配置样例可解析
- [x] M2：KCP 回环通过，丢包测试通过
- [ ] M3：Rust/C++ 双向 TCP 互通，1 小时稳定性
- [ ] M4：UDP + mux + 端口跳跃 + STUN 全部可用
- [ ] M5：relay/tester 可用，性能达标，双平台 CI 绿
