# kcptube Rust 移植进度

> 最后更新：2026-10-31
> 分支：`rs-dev`　基线提交：`a0d7164`（legacy-cpp 迁移 + .gitignore）
> 计划文档：[`docs/migration-plan.md`](migration-plan.md)

---

## 1. 总览

| 指标 | 数值 |
|---|---|
| C++ 参考实现 | 14,308 行（`legacy-cpp/`，含 ikcp 1,109 行） |
| Rust 代码 | 约 3,738 行（15 个 `.rs` 文件） |
| crate 数 | 10（9 个 lib + 1 个 bin） |
| 单元测试 | 48 个，全部通过 |
| 整体完成度 | **M0 完成，M1 完成，M2 待开始** |

```
M0 基线            ██████████ 100%
M1 协议基础层      ██████████ 100%
M2 KCP/传输        ░░░░░░░░░░   0%   ← 下一步
M3 TCP 最小互通     ░░░░░░░░░░   0%
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
| `crates/kcp` | `3rd_party/ikcp.cpp`、`networks/kcp.cpp` | 🟡 10% | 配置与接口已定；核心协议未移植（M2） |
| `crates/config` | `configurations.cpp`（1010 行） | 🟢 100% | 两遍解析、预设展开、MTU、relay 分段、自定义映射、日志路径、错误文本全部对齐 |
| `crates/transport` | `connections.cpp`、`kcp_updater.cpp`、`mux_tunnel.cpp` | 🟡 10% | tokio 版接口骨架；无实现（M2） |
| `crates/client` | `client.cpp`（1917 行） | 🟡 5% | 仅入口 |
| `crates/server` | `server.cpp`（1323 行） | 🟡 5% | 仅入口 |
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

---

## 4. 测试统计

| crate | 测试数 | 覆盖内容 |
|---|---:|---|
| `protocol` | 11 | 位域、字节序、包层/FEC/mux/settings 编解码、截断输入 |
| `crypto` | 12 | 5 种模式向量、往返、篡改检测、IV 展开、CRC32 字节序 |
| `fec` | 7 | GF 表、Botan 向量、往返、缺 share、container |
| `kcp` | 2 | 默认配置、conv |
| `config` | 15 | 样例配置、端口/地址、预设、FEC/MTU、relay、自定义映射、错误文本 |
| `transport` | 1 | 构造 |
| **合计** | **48** | 全部通过 |

命令与结果：

```
cargo fmt --all -- --check        → 0
cargo clippy --workspace --all-targets -- -D warnings → 0
cargo test --workspace            → 48 passed, 0 failed
cargo run -p kcptube -- --check-config service/config_sample_*.conf → No
```

---

## 5. 已知问题与技术债

| # | 问题 | 影响 | 计划 |
|---|---|---|---|
| 1 | `kcp` / `transport` / 三种模式均只有接口 | 无法运行 | M2/M3 |
| 2 | `client/server/relay` 的 `run()` 都返回 `NotImplemented` | 启动会提示未实现 | 随里程碑逐步替换 |
| 3 | `--try` 仅打印“尚未实现” | 连通性测试不可用 | M5 新建 `kcptube-tester` |
| 4 | relay 根配置的 `udp_timeout` 增量等细节尚未在运行层验证 | 仅影响 relay | M5 端到端测试 |
| 5 | `Cargo.lock` 未提交 | 依赖版本不稳定 | 建议提交 |
| 6 | 重新编译 `legacy-cpp/` 需要手动加参数 | 影响互通测试效率 | 见附录 |
| 7 | 协议无版本字段，锁定 `20260329` | 上游升级需同步 | 每次上游发版做 diff 并记录 |
| 8 | `ipv4_only` / `ipv6_only` 的顺序语义照搬 C++（后者覆盖前者） | 同时出现时行为与直觉不同 | 保持兼容，文档说明 |

---

## 6. 下一步（M2 任务清单）

1. [ ] **kcp**：移植 `ikcp.cpp`（send/input/recv/update/flush/check，改版窗口函数）
2. [ ] **kcp**：`SetBandwidth`、`ResetWindowValues`、`WaitQueue*`
3. [ ] **kcp**：与 C++ 版「同一输入序列 → 同一输出包序列」对撞
4. [ ] **transport**：`KcpUpdater`（tokio + 最小堆）
5. [ ] **transport**：TCP/UDP 套接字封装（`socket2`、`SO_REUSEADDR`、`SO_SETFIB`）
6. [ ] **transport**：加密 → 包层 → KCP → UDP 流水线
7. [ ] 测试：回环吞吐/延迟，丢包 10% 下正确性

完成标准：两个进程内 KCP 端点互通，丢包场景不丢数据。

---

## 7. 变更记录

| 日期 | 内容 |
|---|---|
| 2026-10-31 | 完成 M0：C++ 代码迁入 `legacy-cpp/`；建立 Rust workspace 骨架；加密 4 种模式与 Botan 向量对撞（3 种完全一致）；CI 落地；编写计划与进度文档 |
| 2026-10-31 | 完成 M1：配置解析器、协议包层编解码、`chacha20` 8B IV 与 CRC32 对齐、ZFEC 与 `fec_container`；测试从 21 增至 48；`--check-config` 对 4 份样例返回 `No` |

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
