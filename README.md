# kcptube.rs

[KCP Tube](https://github.com/cnbatch/kcptube) 的 Rust 重写版，目标是**与 C++ 版协议逐字节兼容**（当前对齐 `20260329` 版本），可与现有的 kcptube 客户端/服务端混跑。

> 说明：本仓库是社区重写项目，非上游官方。原 C++ 实现完整保留在 [`legacy-cpp/`](legacy-cpp/)，作为协议参考与互操作测试的基准。

## 当前状态

| 模块 | 状态 |
|---|---|
| 协议包层（`crates/protocol`） | 🟢 包层/数据层/mux/settings_wrapper 编解码完成 |
| 加密层（`crates/crypto`） | 🟢 4 种 AEAD + `xor`/`none` 全部与 Botan 3.13 逐字节一致 |
| FEC（`crates/fec`） | 🟢 ZFEC 与 Botan share 逐字节一致，含 `fec_container` |
| KCP（`crates/kcp`） | 🟢 ikcp 全量移植，与 C++ 输出逐字节对撞 |
| 配置解析（`crates/config`） | 🟢 完整解析与校验，`--check-config` 可用 |
| 传输层（`crates/transport`） | 🟢 KCP 调度、加密包层、UDP/TCP 封装、`KcpChannel`、FEC 收发流水线、mux 隧道 |
| 日志（`crates/logging`） | 🟢 messages / status 两类文件已接入；`ip_address.log` 待 STUN/DDNS（M6） |
| client / server / relay | 🟢 client/server 支持 TCP + UDP、单通道与 mux 多路复用、端口跳跃、自定义映射（`[custom_input*]`）与 FEC、保活、会话回收、握手重试；relay 支持两段式握手改写、双向转发、空闲回收、两侧保活、状态日志与 egress 端口跳跃 |
| 连通性测试（`--try`） | 🟢 已实现（`crates/tester`），支持 client/relay 配置 |
| STUN / DDNS | 🟡 计划 M6 |

## 文档

使用文档（基于本仓库 Rust 版代码编写）：

- [使用文档首页](docs/README.md)：模式介绍、命令行用法、配置文件格式速览
- [参数列表](docs/parameters.md)：全部配置项、KCP 预设值、加密与校验、带宽、日志
- [常规配置](docs/client-server.md)：客户端 / 服务端配置示例、端口跳跃、多地址
- [中继模式配置](docs/relay-mode.md)
- [FEC 使用介绍](docs/fec.md)、[MTU 设置](docs/mtu.md)
- [自定义映射](docs/custom-ip-mappings.md)、[具体示例](docs/specific-examples.md)

开发者文档（`docs/dev/`，与使用文档分开放）：

- [开发文档首页](docs/dev/README.md)
- [移植计划](docs/dev/migration-plan.md)：目标、架构、阶段划分、协议关键事实、风险登记
- [移植进度](docs/dev/progress.md)：当前状态、测试统计、已知问题、下一步
- [优化与去重路线](docs/dev/refactor-backlog.md)、[relay 实施方案](docs/dev/relay-plan.md)、[性能基准](docs/dev/benchmark.md)

上游 C++ 版使用文档（仅供参考，行为以本仓库文档为准）：[`legacy-cpp/docs/`](legacy-cpp/docs)

## 目录结构

```text
crates/
  protocol/    线格式：常量、feature/protocol 枚举、包层头
  crypto/      AEAD 模式、SHA3 派生、IV 展开、xor 差分
  fec/         前向纠错（ZFEC）
  kcp/         KCP 传输（移植 ikcp）
  config/      .conf 解析（与 C++ 版兼容）
  transport/   TCP/UDP 会话、KCP 调度器
  logging/     messages / status / ip 日志文件
  client/      客户端模式
  server/      服务端模式
  relay/       中继模式
  tester/      --try 连通性测试
  kcptube/     可执行程序入口
legacy-cpp/    原 C++ 实现（协议参考，勿直接修改）
docs/          用户使用文档（入口 docs/README.md）
docs/dev/      开发者文档：移植计划、进度、重构与 relay 方案
```

## 构建与测试

```bash
cargo build
cargo test --workspace
cargo run -p kcptube -- --help

$env:CARGO_ZIGBUILD_ZIG_PATH = "D:\Program Files\Python313\Lib\site-packages\ziglang\zig.exe"
cargo zigbuild --release --target x86_64-unknown-linux-musl
```



## 兼容性要点

`legacy-cpp/` 中有若干「反直觉」的约定，移植时必须遵守：

- `timestamp` 使用**小端**，而 `sn` / `kcp_conv` / `connection_id` 使用**大端**
- AES-GCM 的 IV 是 **16 字节**（不是常见的 12 字节），Rust 侧需 `AesGcm<Aes256, U16>`
- `xchacha20` 在 C++ 里实际调用的是 Botan 的 `ChaCha20Poly1305` + 24 字节 IV，等价于 XChaCha20
- 每个加密包尾部附 2 字节 `iv_raw`，用于还原 IV（见 `crates/crypto/src/cipher.rs`）
- `chacha20` 的 8 字节 nonce 对应 Botan 的**原始版** ChaCha20-Poly1305（draft-agl），
  与 RFC 8439 的差别只在 Poly1305 长度编码
- `xor` / `none` 的 2 字节校验是大端 CRC32 折叠值（Botan `store_be`）
- 双方系统时间差（绝对值）不得超过 **255 秒**，否则收包端会直接丢弃；两端时钟相差 1 秒以内也会被正常接受
- 包格式没有版本号：任何协议改动都需要两端同时升级

## 许可

本项目（含 `crates/` 下的全部 Rust 代码）以 **GNU General Public License v3.0** 分发，全文见 [`LICENSE`](LICENSE)。

`legacy-cpp/` 是上游 C++ 实现，由 cnbatch 以 **BSD 3-Clause** 授权，其原始许可证与版权声明保留在 [`legacy-cpp/LICENSE`](legacy-cpp/LICENSE)；`legacy-cpp/src/3rd_party/` 中的 ikcp 为 MIT 授权。BSD 3-Clause 与 MIT 均可并入 GPLv3 作品，本项目整体因此以 GPLv3 分发，但上述原始声明不得移除。第三方来源与许可证的完整清单见 [`THIRD-PARTY.md`](THIRD-PARTY.md)。

> 早期以 BSD 3-Clause 发布的版本不受影响，仍可依其原许可证使用；自本许可证变更后的版本起适用 GPLv3。
