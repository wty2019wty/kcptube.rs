# AGENTS.md

kcptube 的 Rust 重写版，目标是与 C++ 版（`legacy-cpp/`，`app_version = 20260329`）**线格式 1:1 兼容**。
计划见 `docs/dev/migration-plan.md`，进度与已知问题见 `docs/dev/progress.md`。

动协议/加密/配置之前先读 `README.md` 的「兼容性要点」：那里是 C++ 版反直觉但必须遵守的约定
（`timestamp` 小端而 `sn`/`kcp_conv`/`connection_id` 大端、AES-GCM IV 为 16 字节、`chacha20` 是原始版 AEAD 等）。

## 构建与验证

```bash
cargo build --workspace
cargo test --workspace                              # 全部（含 7 个真实 socket 端到端测试）
cargo test -p kcptube-crypto                        # 单个 crate
cargo test -p kcptube-crypto vectors                # 单个测试（名称过滤）
cargo test -p kcptube-server --test interop client_server_tcp_roundtrip   # 单个 interop 用例
```

提交前按顺序执行（CI 使用相同命令，`ubuntu` + `windows` 矩阵）：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- 运行：`cargo run -p kcptube -- --help`、`cargo run -p kcptube -- --check-config service/config_sample_client.conf`。
- `--check-config` **永远返回 0**，配置是否合法只看输出行 `Error Found in Configuration File(s): Yes/No`；不要用退出码判断。
- 端到端测试在 `crates/server/tests/interop.rs`，会占用 `127.0.0.1` 的 10000–20000 真实端口并起后台任务；
  用例失败先怀疑端口被占用或启动时序，不要放宽断言或超时。
- 静态发布：`cargo zigbuild --release --target x86_64-unknown-linux-musl`（依赖 cargo-zigbuild，
  Windows 上 Zig 路径用 `CARGO_ZIGBUILD_ZIG_PATH` 指定）。

## 目录与边界

- 纯逻辑 crate：`protocol` / `crypto` / `fec` / `kcp`（零 IO）与 `config`（只解析，无网络）。它们**不得依赖 tokio**。
  `config` 允许依赖 `kcp`，仅用于 `KcpConfig` 的纯参数映射（无 IO、无环）。
- `transport`：tokio 版 asio 层（会话、KCP 调度、加密包层、FEC、mux）。
- `client` / `server` / `relay`：三种运行模式；`kcptube` 是唯一的 bin（clap）。
- 未实现（不要当成 bug 顺手补）：STUN/DDNS、FreeBSD `SO_SETFIB`、性能基准与发布产物。
  `relay` 与 `--try` 已可用，`[custom_input*]` / `ignore_destination_*` 已接入；计划见 `docs/dev/progress.md`。
- `legacy-cpp/`：**只读参考实现**。不要为适配 Rust 或 MinGW 修改其中源码；编译适配放在外部副本或 `build/`。
- `service/`：示例配置与 systemd unit `service/kcptube`（不是二进制）。
- `docs/`：用户使用文档（入口 `docs/README.md`）；`docs/dev/`：开发者文档（移植计划、进度、重构与 relay 方案）；`legacy-cpp/docs/` 是上游文档，只读。

## 依赖与代码约定

- 新依赖加在根 `Cargo.toml` 的 `[workspace.dependencies]`，crate 内写 `xxx.workspace = true`。
- 改加密/FEC 后必须跑通 C++ Botan 对撞测试：`cipher.rs::{vectors_match_botan, chacha20_8byte_nonce_matches_botan, checksum16_matches_botan_crc32}`
  与 `fec::vectors_match_botan_zfec`；新增向量要注明由 C++ 版 Botan 生成。
- 非测试代码禁止 `unwrap()`/`expect()`；`unsafe` 禁用；错误用 `thiserror`；注释与文档用中文。
- 以 C++ 行为为基准，即使它看起来不合理（如空闲 255 秒断开、重传顺序、端口跳跃时机）,并且要说明 。
  有意偏离必须登记到 `docs/dev/progress.md` 第 5 节「已知问题」。
- 程序版本号唯一来源是根 `Cargo.toml` 的 `version`
- 许可证：本项目以 `GPL-3.0-only` 分发，每个 `.rs` 首行固定为 `// SPDX-License-Identifier: GPL-3.0-only`；
  `legacy-cpp/` 保留上游 BSD 3-Clause 声明，**不要**改写其文件头或删除 `legacy-cpp/LICENSE`。
  新增第三方代码须确认与 GPLv3 兼容并登记到 `THIRD-PARTY.md`。
- 提交信息：`<类型>: <中文描述>`，类型用 `feat / fix / refactor / test / docs / ci / chore`；
  开发在 `rs-dev` 分支，`main` 保持与上游可对比。
- 里程碑/行为有进展时同步 `docs/dev/progress.md`（含变更记录），参数语义变化同步 `docs/parameters.md`。
- 源码与文档是 UTF-8 中文，Windows 控制台直接 `Get-Content` 可能乱码；读文件用 read 工具。
