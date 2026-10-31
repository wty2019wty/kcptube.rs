# kcptube.rs

[KCP Tube](https://github.com/cnbatch/kcptube) 的 Rust 重写版，目标是**与 C++ 版协议逐字节兼容**（当前对齐 `20260329` 版本），可与现有的 kcptube 客户端/服务端混跑。

> 说明：本仓库是社区重写项目，非上游官方。原 C++ 实现完整保留在 [`legacy-cpp/`](legacy-cpp/)，作为协议参考与互操作测试的基准。

## 当前状态

| 模块 | 状态 |
|---|---|
| 协议包层（`crates/protocol`） | 🟡 常量/枚举/头长度已对齐，编解码待实现 |
| 加密层（`crates/crypto`） | 🟢 `aes-gcm` / `aes-ocb` / `xchacha20` 已验证与 Botan 输出一致；🟡 `chacha20`(8B IV)、`xor` 待对齐 |
| FEC（`crates/fec`） | 🟡 接口已定，ZFEC 实现待完成 |
| KCP（`crates/kcp`） | 🟡 配置/接口已定，ikcp 移植待完成 |
| 配置解析（`crates/config`） | 🟡 结构体已对齐，解析器待完成 |
| 传输层（`crates/transport`） | 🟡 接口已定（tokio） |
| client / server / relay | 🟡 仅入口 |
| 连通性测试（`--try`） | 🟡 未实现 |

## 文档

- [移植计划](docs/migration-plan.md)：目标、架构、阶段划分、协议关键事实、风险登记
- [移植进度](docs/progress.md)：当前状态、测试统计、已知问题、下一步
- 上游使用文档：[`legacy-cpp/docs/`](legacy-cpp/docs)

## 目录结构

```text
crates/
  protocol/    线格式：常量、feature/protocol 枚举、包层头
  crypto/      AEAD 模式、SHA3 派生、IV 展开、xor 差分
  fec/         前向纠错（ZFEC）
  kcp/         KCP 传输（移植 ikcp）
  config/      .conf 解析（与 C++ 版兼容）
  transport/   TCP/UDP 会话、KCP 调度器
  client/      客户端模式
  server/      服务端模式
  relay/       中继模式
  kcptube/     可执行程序入口
legacy-cpp/    原 C++ 实现（协议参考，勿直接修改）
docs/          使用文档（见 legacy-cpp/docs）
```

## 构建与测试

```bash
cargo build
cargo test --workspace
cargo run -p kcptube -- --help
```



## 兼容性要点

`legacy-cpp/` 中有若干「反直觉」的约定，移植时必须遵守：

- `timestamp` 使用**小端**，而 `sn` / `kcp_conv` / `connection_id` 使用**大端**
- AES-GCM 的 IV 是 **16 字节**（不是常见的 12 字节），Rust 侧需 `AesGcm<Aes256, U16>`
- `xchacha20` 在 C++ 里实际调用的是 Botan 的 `ChaCha20Poly1305` + 24 字节 IV，等价于 XChaCha20
- 每个加密包尾部附 2 字节 `iv_raw`，用于还原 IV（见 `crates/crypto/src/cipher.rs`）
- 双方系统时间差不得超过 **255 秒**
- 包格式没有版本号：任何协议改动都需要两端同时升级

## 许可

BSD 3-Clause。原 C++ 实现版权归 cnbatch 所有，详见 [`LICENSE`](LICENSE)。
