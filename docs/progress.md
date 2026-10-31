# kcptube Rust 移植进度

> 最后更新：2026-10-31
> 分支：`rs-dev`　基线提交：`a0d7164`（legacy-cpp 迁移 + .gitignore）
> 计划文档：[`docs/migration-plan.md`](migration-plan.md)

---

## 1. 总览

| 指标 | 数值 |
|---|---|
| C++ 参考实现 | 14,308 行（`legacy-cpp/`，含 ikcp 1,109 行） |
| Rust 代码 | 约 6,505 行（21 个 `.rs` 文件） |
| crate 数 | 10（9 个 lib + 1 个 bin） |
| 单元/集成测试 | 73 个，全部通过 |
| 整体完成度 | **M0~M2 完成，M3 约 90%** |

```
M0 基线            ██████████ 100%
M1 协议基础层      ██████████ 100%
M2 KCP/传输        ██████████ 100%
M3 TCP 最小互通     █████████░  90%   ← 当前
M4 完整模式         ░░░░░░░░░░   0%
M5 relay/跨平台     ░░░░░░░░░░   0%
```

---

## 2. 各模块状态

| 模块 | C++ 对应 | 状态 | 说明 |
|---|---|---|---|
| `crates/protocol` | `connections.hpp` 包结构、`share_defines.hpp` 常量 | 🟢 100% | 常量、枚举、包层/数据层/mux/settings_wrapper 编解码全部完成并有测试 |
| `crates/crypto` | `aead.hpp`、`data_operations.cpp`、`simple_hashing.hpp` | 🟢 100% | 5 种加密模式全部与 Botan 向量逐字节一致（含 `chacha20` 8B IV 的原始版 AEAD） |
| `crates/fec` | `data_operations.cpp` + Botan ZFEC | 🟢 100% | ZFEC 矩阵/GF(2^8)/编解码已移植，share 与 Botan 3.13 逐字节一致；含 `fec_container` 打包 |
| `crates/kcp` | `3rd_party/ikcp.cpp`、`networks/kcp.cpp` | 🟢 100% | ikcp 全量移植；发送/重传/拥塞/窗口探测与 C++ 输出逐字节对撞通过 |
| `crates/config` | `configurations.cpp`（1010 行） | 🟢 100% | 两遍解析、预设展开、MTU、relay 分段、自定义映射、日志路径、错误文本全部对齐 |
| `crates/transport` | `connections.cpp`、`kcp_updater.cpp`、`mux_tunnel.cpp` | 🟢 90% | KCP 调度、加密包层、UDP/TCP 封装、`KcpChannel` 端到端流水线已完成；mux/FEC 钩子留待 M4 |
| `crates/client` | `client.cpp`（1917 行） | 🟢 70% | TCP 监听、握手、`raw_data` 双向转发、保活发送已实现；端口跳跃/mux/UDP 未做 |
| `crates/server` | `server.cpp`（1323 行） | 🟢 70% | UDP 监听、握手、目标 TCP 连接与双向转发已实现；STUN/DDNS/FEC/mux 未做 |
| `crates/relay` | `relay.cpp`（1802 行） | 🟡 5% | 仅入口；配置已能拆出 ingress/egress |
| `crates/kcptube` | `main.cpp` | 🟢 90% | clap 参数、usage、`--check-config` 可用；`--try` 仍为骨架 |
| `--try` 连通性测试 | `tester.cpp`（392 行） | ❌ 0% | 未开始，计划 M5，需新建 `kcptube-tester` crate |

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

**FEC**（Botan `ZFEC(4,6)`，输入 `00 01 … 0F`）：

| share | Botan 输出 |
|---|---|
| 4 | `88 89 8A 8B` |
| 5 | `24 25 26 27` |

Rust 输出完全一致，见 `crates/fec/src/lib.rs::vectors_match_botan_zfec`。

### 3.5 KCP 与传输层（M2，进行中）

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
- [ ] FEC / mux 钩子（按计划放到 M4）

> 已知差异：C++ 的重传顺序依赖 `std::unordered_map` 迭代顺序（不可移植），
> Rust 版固定按 SN 升序发送；报文内容与集合完全一致，仅同一次 flush 内的顺序不同。
>
> `SO_SETFIB` 是 FreeBSD 专有选项，随 M5 平台适配补充。

### 3.6 TCP 最小互通（M3，进行中）

- [x] **server**：UDP 监听（`listen_on` + 首个 `listen_port`）
- [x] **server**：`conv=0` 握手分流、`initialise` 解析、随机分配 conv
- [x] **server**：连接目标 TCP，双向 `raw_data` 转发
- [x] **server**：按数据包源地址更新回包目标（C++ 客户端握手后会切换本地 UDP 端口）
- [x] **client**：本地 TCP 监听，每个连接独立 UDP socket + 握手
- [x] **client**：握手响应解析，切换到服务端分配的 conv
- [x] **client**：TCP ↔ `raw_data` 双向转发，保活发送/响应
- [x] **加密**：默认 `aes-gcm` 全流程
- [x] 互通验证：
  - Rust 客户端 → **C++ 服务端**：3 次 TCP echo 往返通过
  - **C++ 客户端** → Rust 服务端：3 次 TCP echo 往返通过
  - Rust 客户端 → Rust 服务端：自动化集成测试 `crates/server/tests/interop.rs`
- [ ] 连续 1 小时稳定性测试
- [ ] 日志格式与 C++ 版对齐（status / messages / ip 三个文件）

> C++ 参考版编译使用 `legacy-cpp/` 的临时副本（仅 `dns_helper.cpp` 做 MinGW 兼容补丁），
> 未修改仓库内的 `legacy-cpp/`。

---

## 4. 测试统计

| crate | 测试数 | 覆盖内容 |
|---|---:|---|
| `protocol` | 11 | 位域、字节序、包层/FEC/mux/settings 编解码、截断输入 |
| `crypto` | 12 | 5 种模式向量、往返、篡改检测、IV 展开、CRC32 字节序 |
| `fec` | 7 | GF 表、Botan 向量、往返、缺 share、container |
| `kcp` | 13 | C++ 逐字节 transcript、分片、ACK、RTO、10% 丢包回环、窗口/带宽 |
| `config` | 15 | 样例配置、端口/地址、预设、FEC/MTU、relay、自定义映射、错误文本 |
| `transport` | 13 | 调度器、加密包层、UDP/TCP 封装、双通道 UDP 端到端 |
| `server` | 2 | 端口连续性单测 + `interop.rs` 端到端（Rust client ↔ Rust server ↔ echo TCP） |
| `client` | 0 | 由 `server/tests/interop.rs` 覆盖 |
| **合计** | **73** | 全部通过 |

命令与结果：

```
cargo fmt --all -- --check        → 0
cargo clippy --workspace --all-targets -- -D warnings → 0
cargo test --workspace            → 73 passed, 0 failed
cargo run -p kcptube -- --check-config service/config_sample_*.conf → No
```

---

## 5. 已知问题与技术债

| # | 问题 | 影响 | 计划 |
|---|---|---|---|
| 1 | 只监听第一个 `listen_port` | 端口范围/端口跳跃不可用 | M4 |
| 2 | 仅支持 TCP `raw_data` | UDP 转发、mux 不可用 | M4 |
| 3 | FEC / mux 未接入收发流水线 | 只能关闭 FEC 运行 | M4 |
| 4 | relay 仍是骨架 | relay 模式不可用 | M5 |
| 5 | `--try` 仅打印“尚未实现” | 连通性测试不可用 | M5 |
| 6 | 握手/超时清理、状态日志格式未完全对齐 | 长时间运行行为待验证 | M3 收尾 / M4 |
| 7 | `Cargo.lock` 未提交 | 依赖版本不稳定 | 建议提交 |
| 8 | 协议无版本字段，锁定 `20260329` | 上游升级需同步 | 每次上游发版做 diff 并记录 |
| 9 | `ipv4_only` / `ipv6_only` 的顺序语义照搬 C++ | 同时出现时行为与直觉不同 | 保持兼容 |
| 10 | `SO_SETFIB`（FreeBSD）未实现 | FreeBSD 多路由表不可用 | M5 |

---

## 6. 下一步（M3 收尾 → M4）

1. [ ] 连续 1 小时稳定性与断线重连测试
2. [ ] 握手重试、超时清理、dead link 清理与 C++ 对照
3. [ ] 状态日志（status / messages / ip）格式对齐
4. [ ] **UDP 转发**（`udp_timeout`、UDP 映射表）
5. [ ] **mux**：`mux_tunnels`、`mux_transfer` / `mux_cancel` / `pre_connect_custom_address`
6. [ ] **端口跳跃**：`dport_refresh`、`test_connection`、socket 切换
7. [ ] FEC 接入发送/接收流水线
8. [ ] STUN / DDNS / 带宽限制

完成标准：M4 验收表中的 UDP、mux、端口跳跃全部可用。

---

## 7. 变更记录

| 日期 | 内容 |
|---|---|
| 2026-10-31 | 完成 M0：C++ 代码迁入 `legacy-cpp/`；建立 Rust workspace 骨架；加密 4 种模式与 Botan 向量对撞（3 种完全一致）；CI 落地；编写计划与进度文档 |
| 2026-10-31 | 完成 M1：配置解析器、协议包层编解码、`chacha20` 8B IV 与 CRC32 对齐、ZFEC 与 `fec_container`；测试从 21 增至 48；`--check-config` 对 4 份样例返回 `No` |
| 2026-10-31 | M2 主体：移植 ikcp（发送/重传/拥塞/窗口探测）、与 C++ 输出逐字节对撞、10% 丢包回环；`KcpUpdater` 最小堆调度与 `PacketCodec`；测试增至 65 |
| 2026-10-31 | 完成 M2：UDP/TCP 封装、`KcpChannel` 端到端流水线（本机 UDP + AES-GCM 实测）；测试增至 71 |
| 2026-10-31 | M3：客户端/服务端握手与 TCP `raw_data` 双向转发；Rust↔Rust 集成测试、Rust 客户端↔C++ 服务端、C++ 客户端↔Rust 服务端均通过；测试增至 73 |

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
