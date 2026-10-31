# kcptube Rust 移植进度

> 最后更新：2026-10-31
> 分支：`rs-dev`　基线提交：`a0d7164`（legacy-cpp 迁移 + .gitignore）
> 计划文档：[`docs/migration-plan.md`](migration-plan.md)

---

## 1. 总览

| 指标 | 数值 |
|---|---|
| C++ 参考实现 | 14,308 行（`legacy-cpp/`，含 ikcp 1,109 行） |
| Rust 代码 | 约 1,538 行（14 个 `.rs` 文件） |
| crate 数 | 10（9 个 lib + 1 个 bin） |
| 单元测试 | 21 个，全部通过 |
| 整体完成度 | **M0 完成，M1 进行中（约 15%）** |

```
M0 基线            ██████████ 100%
M1 协议基础层       ██░░░░░░░░  15%   ← 当前
M2 KCP/传输         ░░░░░░░░░░   0%
M3 TCP 最小互通     ░░░░░░░░░░   0%
M4 完整模式         ░░░░░░░░░░   0%
M5 relay/跨平台     ░░░░░░░░░░   0%
```

---

## 2. 各模块状态

| 模块 | C++ 对应 | 状态 | 说明 |
|---|---|---|---|
| `crates/protocol` | `connections.hpp` 包结构、`share_defines.hpp` 常量 | 🟡 30% | 常量、`Feature`/`ProtocolType`、头长度、字节序工具已实现并有测试；**包编解码未实现** |
| `crates/crypto` | `aead.hpp`、`data_operations.cpp`、`simple_hashing.hpp` | 🟢 70% | 密钥/IV 派生、IV 展开、AES-GCM/OCB/XChaCha、xor 差分已完成；`chacha20`(8B IV) 待对齐；与 Botan 向量对撞通过 |
| `crates/fec` | `data_operations.cpp` + Botan ZFEC | 🟡 15% | 参数校验与接口已定；编码/解码返回 `NotImplemented` |
| `crates/kcp` | `3rd_party/ikcp.cpp`、`networks/kcp.cpp` | 🟡 10% | 配置与接口已定；核心协议未移植 |
| `crates/config` | `configurations.cpp`（1010 行） | 🟡 20% | `UserSettings` 字段已与 C++ 对齐；解析器返回 `NotImplemented` |
| `crates/transport` | `connections.cpp`、`kcp_updater.cpp`、`mux_tunnel.cpp` | 🟡 10% | tokio 版接口骨架；无实现 |
| `crates/client` | `client.cpp`（1917 行） | 🟡 5% | 仅入口 |
| `crates/server` | `server.cpp`（1323 行） | 🟡 5% | 仅入口 |
| `crates/relay` | `relay.cpp`（1802 行） | 🟡 5% | 仅入口；ingress/egress 尚未拆分 |
| `crates/kcptube` | `main.cpp` | 🟢 80% | clap 参数、usage、`--try`/`--check-config` 骨架；配置解析未完成 |
| `--try` 连通性测试 | `tester.cpp`（392 行） | ❌ 0% | 未开始，计划 M5，需新建 `kcptube-tester` crate |

---

## 3. 已完成项（M0）

### 3.1 环境与工具链

- [x] Rust 1.96（`x86_64-pc-windows-gnu`），依赖源：官方 crates.io
- [x] MSYS2 GCC 16.2 + CMake 4.4.5 + asio 1.34 + Botan 3.13
- [x] C++ 参考实现编译成功并能运行（`kcptube version 20260329`）

### 3.2 工程骨架

- [x] workspace（`resolver = 3`，edition 2024）
- [x] 10 个 crate 的目录、`Cargo.toml`、模块划分
- [x] 共享依赖清单（`[workspace.dependencies]`）
- [x] `.gitignore` 增加 `target/`
- [x] CI：fmt / clippy（`-D warnings`）/ test，ubuntu + windows 矩阵
- [x] 新 `README.md`（Rust 版说明），原文档在 `legacy-cpp/`

### 3.3 协议与加密的实证结论

**加密向量对撞**（key = `[0x01;32]`，IV = `[0x02;N]`，AAD = `"KCP PortHopping"`，明文 `hello kcptube!!`）：

| 模式 | IV 长度 | Botan 输出（前 16 字节） | Rust | 结论 |
|---|---:|---|---|---|
| `aes-gcm` | 16 | `1853FB26B0E14477A…` | 相同 | ✅ 完全一致 |
| `aes-ocb` | 12 | `C581B50BAB10C402F…` | 相同 | ✅ 完全一致 |
| `xchacha20` | 24 | `A4035A83C557972ED…` | 相同 | ✅ 完全一致 |
| `chacha20` | 12 | `8E799E42E28383B90…` | 相同 | ✅ 完全一致 |
| `chacha20` | 8 | `C80B52B6…F81C62` | `C80B52B6…F66AA4` | ⚠️ 密文相同，**tag 不同** |

完整向量见 `crates/crypto/src/cipher.rs::vectors_match_botan`。

**其它已确认的协议事实**（已写入计划文档第 5 节）：

- `timestamp` 小端，`sn` / `kcp_conv` / `connection_id` 大端
- 时间差容忍上限 255 秒
- 包尾 2 字节 `iv_raw`，用于还原每包 IV
- 配置解析对 key/value 统一转小写（`AES-GCM` 合法）

---

## 4. 测试统计

| crate | 测试数 | 覆盖内容 |
|---|---:|---|
| `protocol` | 5 | 常量推导、位域布局、feature 枚举值、头长度、字节序 |
| `crypto` | 10 | 向量对撞、AEAD 往返、篡改检测、xor 可逆、IV 展开、模式解析 |
| `fec` | 2 | 参数校验、禁用状态 |
| `kcp` | 2 | 默认配置、conv |
| `config` | 1 | 默认值与 C++ 对齐 |
| `transport` | 1 | 构造 |
| **合计** | **21** | 全部通过 |

命令与结果：

```
cargo fmt --all -- --check        → 0
cargo clippy --workspace --all-targets -- -D warnings → 0
cargo test --workspace            → 21 passed, 0 failed
```

---

## 5. 已知问题与技术债

| # | 问题 | 影响 | 计划 |
|---|---|---|---|
| 1 | `chacha20` 8 字节 IV 的 Poly1305 tag 与 Botan 不一致（密文一致） | 该模式无法互通 | M1 对齐，或文档标注暂不支持 |
| 2 | ZFEC 未实现，且 `zfec-rs` 仅 0.1 版 | FEC 功能不可用 | M1，先做向量对撞再启用 |
| 3 | `config` 解析器未实现（返回 `NotImplemented`） | 无法读取真实配置 | M1 优先 |
| 4 | `protocol` 只有结构定义，没有编解码 | 无法收发数据 | M1 |
| 5 | `kcp` / `transport` / 三种模式均只有接口 | 无法运行 | M2/M3 |
| 6 | `client/server/relay` 的 `run()` 都返回 `NotImplemented` | `cargo run -- config.conf` 会提示未实现 | 随里程碑逐步替换 |
| 7 | `Relay::new` 在 `main.rs` 中用同一份配置占位 | relay 无法真实工作 | 配置解析支持 `[listener]`/`[forwarder]` 后修正 |
| 8 | `EncryptionMode::from_str` 区分大小写 | 直接调用会拒绝 `AES-GCM` | 在 `config` 层先 `to_lowercase()`（C++ 行为），已记录 |
| 9 | `Cargo.lock` 未提交 | 依赖版本不稳定 | 建议随骨架一起提交 |
| 10 | 重新编译 `legacy-cpp/` 需要手动加参数 | 影响互通测试效率 | 见下方「附录：C++ 参考版编译」 |
| 11 | 协议无版本字段，锁定 `20260329` | 上游升级需同步 | 每次上游发版做 diff 并记录 |

---

## 6. 下一步（M1 任务清单）

按依赖顺序：

1. [ ] `config`：实现解析器（先跑通 `service/config_sample_*.conf`）
   - 两遍解析（`mode` 优先）
   - key/value 转小写、注释、`->` 映射、端口范围/列表
   - kcp 预设展开（`regular1-5`、`fast1-6`、`manual`）
   - `[listener]` / `[forwarder]`（relay）
2. [ ] `protocol`：包层编解码（`packet_layer*`、`data_layer`、`settings_wrapper`）
3. [ ] `crypto`：补齐 `xor` / `none` 的 CRC32 对齐 + `chacha20` 8B tag
4. [ ] `fec`：ZFEC 实现与向量验证
5. [ ] 建立 `tests/vectors/`：由 C++ 程序生成、Rust 端消费的固定向量

完成标准：`cargo test --workspace` 覆盖 M1 全部模块；`--check-config` 对样例配置返回 `No`。

---

## 7. 变更记录

| 日期 | 内容 |
|---|---|
| 2026-10-31 | 完成 M0：C++ 代码迁入 `legacy-cpp/`；建立 Rust workspace 骨架；加密 4 种模式与 Botan 向量对撞（3 种完全一致）；CI 落地；编写计划与进度文档 |

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
