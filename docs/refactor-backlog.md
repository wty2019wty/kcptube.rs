# Rust 版优化与去重路线

> 面向开发者的内部计划，记录跨模块重复审计、分阶段重构顺序与已拍板决策。
> 项目进度总表仍见 [移植进度](progress.md)，兼容性约束见根目录 `AGENTS.md`。

## 1. 背景

- 第一轮重构已完成：`kcp`、`client`、`server` 三个 1000+ 行文件按职责拆分为子模块
  （`kcp/src/lib.rs` 1354 → 204、`client/src/lib.rs` 1541 → 212、`server/src/lib.rs` 1300 → 145）。
- 第二轮审计聚焦**跨模块可复用性与冗余**。结论：Rust 侧存在若干逐字重复；
  C++ 原版（`legacy-cpp/src/modes/`）的冗余更严重，但其中一部分是 Rust 尚未接入的能力
  （`KcpUpdater`、`WaitQueue*`），不能按普通死代码处理。
- 本路线目标：先做等价重构消除重复，再做有测量的性能优化，最后用公共层承接 relay 移植。

## 2. 审计结论

### 2.1 完全重复（阶段 1 处理）

| 内容 | Rust 位置 | 建议归属 |
|---|---|---|
| `handshake_config(mtu)` | `client/src/util.rs`、`server/src/util.rs` | `KcpConfig::handshake(mtu)`（`kcptube-kcp`） |
| `data_config(settings, conv)` | 同上 | `UserSettings::kcp_config(conv)`（`kcptube-config`） |
| `fec_pair(settings)` | 同上 | `UserSettings::fec_parameters()`，内部复用 `fec_enabled()` |
| `is_connection_reset(err)` | `client/src/util.rs`、`server/src/util.rs` | `kcptube-transport` |
| `wildcard_addr(peer)` | `client/src/util.rs`；`server/src/handshake.rs`、`server/src/mux.rs` 各有一份内联 v4/v6 选择 | `transport::udp::wildcard_addr` |
| `log_message()`（`tracing::warn` + 带时间戳写文件） | `client/src/state.rs`、`server/src/state.rs` | `kcptube_logging::log_warning(path, msg)` |
| 状态摘要拼串 / 写文件 / `println!` | `client/src/state.rs::maybe_log_status`、`server/src/status.rs::maybe_log_status` | `kcptube_logging::format_status_summary()` / `print_status_summary()` |
| 通道初始化 `set_blast + set_bandwidth + set_rx_minrto(10)`（数据通道再加 `reset_window_values` + `update`） | `client/src/handshake.rs` ×2、`server/src/handshake.rs` ×2、`client/src/port_hop.rs` ×1 | `KcpChannel` 的初始化方法 |
| `log_status` 判空 | 客户端 `lib.rs::run`、服务端 `status.rs` 手写 `as_os_str().is_empty()` | 直接使用已有的 `UserSettings::has_status_log()` |

### 2.2 结构性重复（阶段 2 处理）

| 内容 | 位置 | 差异 |
|---|---|---|
| mux TCP 桥接（writer 任务 + reader 任务 + `remove_record` + `send_cancel`） | `client/src/mux.rs::mux_handle_tcp`、`server/src/mux.rs::create_mux_local` 的 TCP 分支 | 服务端统计 egress 字节、关闭时 `mark_expiring` |
| 控制帧处理（`KeepAlive` / `KeepAliveResponse` / `Failure` / `Disconnect`） | `client/src/tcp.rs`、`client/src/udp.rs`、`server/src/data.rs::drain_data`、`transport/src/mux.rs::run` | 数据面落地端不同，控制面相同 |
| mux runner 启动（`MuxTunnel::run` + `clear_records` + 事件循环） | `client/src/state.rs::spawn_mux_tunnel`、`server/src/handshake.rs::establish_mux` | client 注册状态通道，server 注册会话并跑事件循环 |

### 2.3 死代码与未接入（阶段 3 / 4 处理）

| 项 | 位置 | 性质 |
|---|---|---|
| `KcpUpdater` 整个调度器 | `transport/src/lib.rs` | **Rust 未接入**；C++ 在 client/server/relay 中大量使用。计划阶段 4 接入，不直接删除 |
| `Kcp::wait_queue_*` ×5 | `kcp/src/api.rs` | C++ 用于 TCP 背压；Rust 改用 `mpsc` 背压（`progress.md` 3.3 已登记差异），保留接口但暂不接入 |
| `Kcp::update_check` | `kcp/src/timer.rs` | 未使用；与 `refresh` 二选一，确认后清理 |
| `KcpChannel::{keep_alive_send_time, keep_alive_response_time, fec_enabled, recv_and_process}` | `transport/src/channel.rs` | 纯 Rust 侧未使用，可直接清理 |
| `transport::resolve_endpoint` | `transport/src/lib.rs` | 未使用；`resolve_endpoints_filtered` 已覆盖 |
| `MuxTunnel::record_count`、`UdpEndpoint::as_socket/from_socket` | `transport/src/{mux,udp}.rs` | 未使用 |
| `StatusCounters::add_egress_raw` | `logging/src/lib.rs` | 未使用 |
| `FecSender::generated_shares`、`FecReceiver::recovery_threshold` | `fec/src/lib.rs` | 未使用 |
| `ClientError::NotImplemented`、`ServerError::NotImplemented` | `client/src/lib.rs`、`server/src/lib.rs` | 从未构造；relay 自己的保留 |
| `ProtocolError::error_message`、`DataLayer::{encode_packet, decode_packet}` 等 | `protocol/src/packet.rs` | 需逐个确认，避免误删对撞测试依赖 |

### 2.4 与 C++ 原版对照

C++ 原版同样冗余，且是「按 mode 复制」：

- `log_status` / `loop_get_status`、`cleanup_expiring_handshake_connections`、
  `cleanup_expiring_data_connections`、`loop_find_expires`、`loop_keep_alive`、
  `udp_listener_incoming(_unpack)`、`udp_forwarder_incoming(_unpack)`、`fec_maker`、
  `fec_find_missings`、`data_sender`、`kcp_sender`、`verify_kcp_conv` 等
  在 `client.cpp` / `server.cpp` / `relay.cpp` 各有一份；部分在 `tester.cpp` 还有第四份。
- 每个 mode 的 `loop_get_status` 内部用 `#ifdef __cpp_lib_format` 把同一格式串写两次。
- KCP 通道初始化 `SetMTU + NoDelay + SetBandwidth (+ RxMinRTO=10)` 在四种 mode 中约 10 处重复。
- `data_operations.cpp::encrypt_decrypt<T>` 的 6 个方法重复同样的缓存查找样板。

同时，C++ 在另一些地方做得比 Rust 好，值得保留其设计意图：

- `share_defines` 提供 `generate_random_number`、`is_continuous`、`to_speed_unit`、
  `fec_initialse` 等公共函数；
- `KcpUpdater` 是真正的截止时间调度器，被所有 mode 使用；
- `WaitQueue*` 被 `pause()/resume` 用于 TCP 背压。

结论：把 client/server 之间的重复下沉到 `transport` / `logging` / `config`，
符合上游的分层意图，不是过度设计；但 `KcpUpdater` 和 `WaitQueue*` 是能力差异，
删除前必须登记或补接线。

## 3. 优化原则

1. **行为不变优先**：阶段 1–3 是等价重构，不改变线格式、超时、日志文本。
2. **每阶段独立可回滚**，按 AGENTS.md 的提交信息格式单独提交。
3. **以 C++ 为基准**：纯抽取不改变语义；有意偏离登记到 `docs/progress.md`。
4. **不动的部分**：
   - `protocol/src/packet.rs` 各包层编解码（逐字段对应 C++ packed struct）；
   - `config/src/parser.rs` 的解析分支（逐行对应 C++ 函数）；
   - `kcp` 算法内部（已有 C++ 逐字节对撞向量）；
   - `crypto`（已有 Botan 向量）。
5. **验收命令**（每阶段提交前）：
   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

## 4. 已拍板决策

| 编号 | 决策 | 说明 |
|---|---|---|
| D1 | `KcpUpdater` **接入**而不是删除 | C++ 原版在用；固定 5ms 轮询是 Rust 侧性能债，放到阶段 4 |
| D2 | `kcptube-config` 允许依赖 `kcptube-kcp` | 仅用于纯函数 `kcp_config()` 映射，无 IO、无环；同步更新 `AGENTS.md` 中对 config crate 的描述 |
| D3 | `wait_queue_*` 保留 | 对应 C++ 背压；Rust 的 mpsc 替代方案已在 `progress.md` 3.3 登记 |
| D4 | 协议包层与配置解析器不重构 | 保留与 C++ 的可追溯性，优先对撞测试 |
| D5 | 状态日志阶段 1 保持逐字不变 | C++ 的 `mux_tunnels:` 段落 Rust 尚未输出，作为单独行为对齐任务，不混入重构 |

## 5. 阶段计划

### 5.0 实施顺序（先完成重写还是先优化）

结论：**先完成剩余重写（M5 relay/`--try`、M6 STUN/DDNS），但阶段 1 与阶段 5 的前置抽取
必须插到 relay 之前**；纯性能优化（阶段 4）放到功能完成并有基线数据之后。

| 顺序 | 内容 | 性质 | 说明 |
|---|---|---|---|
| 1 | 阶段 1 公共层收口 | 等价重构 | relay 会复用配置/日志/通道初始化，先做可避免第三份复制 |
| 2 | 阶段 5 前置：端口跳跃、保活、握手泛化 | 等价重构 | relay 的 ingress/egress 都需要 |
| 3 | relay + `--try`（M5 主体） | 功能 | 补上用户可见的最大能力缺口 |
| 4 | STUN / DDNS / `ip_address.log`（M6） | 功能 | 相对独立，风险低 |
| 5 | 阶段 2 剩余结构去重 + 阶段 3 死代码 | 等价重构 | 此时 client/server 形态已稳定 |
| 6 | 阶段 4 性能优化 | 性能 | 基于 M5 的基准数据，避免为错误热点优化 |

relay 的具体设计与分步实施见 [`relay-plan.md`](relay-plan.md)（R0–R5）。

例外：如果线上已经出现「空闲 CPU 过高 / 会话数一多就卡」的可观测问题，
可以把阶段 4.1（KcpUpdater deadline 驱动）提前到 relay 之前——它同时能让 relay 不再新增
一套固定 5ms 轮询。

不在重写前做的事：每包分配优化、`Bytes`/buffer pool 等微优化。
relay 会改变流量模型，等形态固定后再测量更可靠。

### 阶段 1：公共层收口（预估 1–2 天，低风险）

- [ ] `transport`：
  - 新增 `is_connection_reset(&io::Error)`（放在 `lib.rs` 或 `udp.rs` 并导出）；
  - 新增 `udp::wildcard_addr(SocketAddr)`；
  - `KcpChannel` 增加：
    - `set_rx_minrto(value)`；
    - `apply_handshake_tuning(blast, outbound, inbound)`；
    - `apply_data_tuning(blast, outbound, inbound, srtt)`（含 `reset_window_values` 与一次 `update`）。
- [ ] `config`：
  - 新增 `UserSettings::kcp_config(conv)`、`fec_parameters()`；
  - 客户端/服务端改用 `has_status_log()`；
  - 删除 `client/src/util.rs`、`server/src/util.rs` 中的 `handshake_config` / `data_config` / `fec_pair`。
- [ ] `logging`：
  - 新增 `log_warning(log_messages, message)`；
  - 新增 `format_status_summary(...)` / `print_status_summary(...)`，参数包括
    `config_filename`、四项速率、`fec_recover`、`latencies`；
  - `client/src/state.rs` 与 `server/src/status.rs` 改为调用。
- [ ] `KcpChannel` 初始化替换 4 处调用点。
- [ ] 新增测试：
  - `UserSettings::kcp_config` 与 C++ 预设值对照；
  - 状态摘要格式快照（固定输入 → 固定字符串）；
  - `fec_parameters` 与 `fec_enabled` 一致性。

**验收**：上述符号全库只剩一份；106 个测试全绿；状态日志与重构前逐字一致。

### 阶段 2：结构性去重（预估 2–3 天，中风险）

- [ ] `transport::mux::spawn_tcp_bridge(...)`：合并 client/server 的 TCP 读写任务骨架。
      参数化：计数器（可选）、关闭策略（client 发 `send_cancel`；server 额外 `mark_expiring`）。
- [ ] 控制帧处理：抽 `handle_control_feature()`（`KeepAlive` 回包、`KeepAliveResponse` 打点、
      `Failure` / `Disconnect` 关闭），返回「是否关闭对端」；`RawData` 分支保留在各自模块。
- [ ] `MuxTunnel::spawn_runner(...)`：统一「驱动 `run` + `clear_records` + 事件循环」的启动与退出。
- [ ] 完整事件循环（client 分发 vs server 建本地连接）不合并。
- [ ] 回归：mux 相关 interop（100 并发 TCP、`pre_connect_custom_address`、mux+FEC）。

**验收**：mux 压测与全部 interop 通过；client/server 的 `mux.rs` 不再包含重复的读写任务骨架。

### 阶段 3：死代码清理与偏差登记（预估 0.5–1 天，低风险）

- [ ] 清理 2.3 表中「纯 Rust 侧未使用」的项；
- [ ] `KcpUpdater` / `wait_queue_*`：暂不删除，在 `docs/progress.md` 明确标注
      「已实现未接入」/「有意偏离」；
- [ ] 确认 `protocol` 与 `fec` 的可删除项（先跑对撞测试）。

**验收**：`cargo clippy -D warnings` 无新增告警；测试数量只减不增（删除死 API 的测试若存在则同步删除）。

### 阶段 4：性能优化（预估 3–5 天，中高风险）

先测量，再改：

- [ ] **基线**：记录空闲 1000 会话下的 CPU 秒数、单核吞吐、唤醒次数
      （可用 `--release` + 简单脚本；必要时新增 `scripts/bench_idle.ps1`）。
- [ ] **4.1 KCP 驱动改为 deadline 驱动**（最大收益）：
  - 服务端 `SocketState::tick_kcp` 用现有的 `KcpUpdater` 替换固定 5ms 全表扫描：
    创建会话时 `schedule`，关闭时 `remove`，`run_driver` 用 `sleep_until(next_deadline)`；
  - 客户端非 mux 通道：把 `interval(5ms)` 改为 `sleep_until(channel.check(now))`，
    或复用 `KcpUpdater`（需在 `select!` 中同时等待 socket / reader）；
  - `MuxTunnel::run` 同样改为 deadline 驱动；
  - 验证：10% 丢包回环、C++ 对撞向量、interop 端口跳跃/长连接。
- [ ] **4.2 减少每包分配与拷贝**（测量后再做）：
  - `create_data_packet` → `Kcp::send` → `flush` → 加密，当前每层都有 `Vec` 分配；
  - 评估 buffer pool / `Bytes` / `KcpChannel` 就地写入接口；
  - 优先处理小包高频路径，不为省一次分配引入复杂生命周期。
- [ ] **4.3 批量收包**：`drain_data` 在单次加锁内取空，减少锁竞争。
- [ ] 更新基线与结论到本文档。

**验收**：空闲 CPU / 唤醒次数有可量化下降；功能测试与 C++ 互通不回退。

### 阶段 5：relay 移植前置（预估 2–3 天，中风险）

C++ `relay.cpp` 是 client 逻辑的翻版（内部还有 `via_listener` / `via_forwarder` 双份），
因此先泛化再移植：

- [ ] 端口跳跃（`client/src/port_hop.rs`）参数化：注入目标选择器与角色（client / relay ingress / relay egress）；
- [ ] 保活与空闲清理抽为可复用任务；
- [ ] 握手通道建立抽为可复用函数；
- [ ] 再实现 relay，按方向参数化，不复制第三份。

**验收**：relay 实现本身不再包含与 client 重复的状态机。

## 6. 风险与回滚

| 风险 | 缓解 |
|---|---|
| deadline 驱动改变重传时机 | 保留 10% 丢包回环、`output_matches_cpp_transcript`、`retransmission_matches_cpp_packets`；先只改服务端驱动，观察一轮 |
| mux 桥接抽取改变关闭顺序 | 单独提交；跑 100 并发 mux 与 `pre_connect` 用例；对比关闭日志顺序 |
| 状态摘要抽取改变输出 | 加字符串快照测试；阶段 1 不允许任何格式变化 |
| config 新增 kcp 依赖引发分层争议 | 只放纯映射函数；同步更新 AGENTS.md 的边界描述 |
| 误删 `KcpUpdater` / `wait_queue_*` | 已在本文档与 `progress.md` 标注，阶段 4 前不删 |

## 7. 进度跟踪

| 阶段 | 状态 | 备注 |
|---|---|---|
| 1 公共层收口 | 待开始 | |
| 2 结构性去重 | 待开始 | |
| 3 死代码清理 | 待开始 | |
| 4 性能优化 | 待开始 | 依赖 D1 |
| 5 relay 前置 | 待开始 | 依赖阶段 1–2 |

## 8. 变更记录

| 日期 | 内容 |
|---|---|
| 2026-11-01 | 建立本文档：记录跨模块重复审计、与 C++ 原版对照、分阶段优化路线与决策 D1–D5；补充 5.0 实施顺序（先完成 M5/M6，阶段 1/5 作为 relay 前置，阶段 4 后置） |
