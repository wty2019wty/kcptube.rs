# AGENTS.md

kcptube 的 Rust 重写版，目标是与 C++ 版（`legacy-cpp/`，`app_version = 20260329`）**线格式 1:1 兼容**。
计划见 `docs/migration-plan.md`，进度与已知问题见 `docs/progress.md`。

## 构建与验证

```bash
cargo build --workspace
cargo test --workspace                    # 全部
cargo test -p kcptube-crypto              # 单个 crate
cargo test -p kcptube-crypto vectors      # 单个测试（名称过滤）
```

提交前按顺序执行（CI 使用相同命令）：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

运行：`cargo run -p kcptube -- --help`、`cargo run -p kcptube -- --check-config service/config_sample_client.conf`。

## 目录与边界

- `crates/protocol|crypto|fec`：纯逻辑、零 IO，**不得依赖 tokio**。
- `crates/config`：`.conf` 解析，行为必须对齐 `legacy-cpp/src/shares/configurations.cpp`。
- `crates/transport`：tokio 版 asio 层（会话、KCP 调度、mux）。
- `crates/kcptube`：CLI 入口（clap），usage 输出与 C++ 版一致。
- `legacy-cpp/`：**只读参考实现**。不要为了适配 Rust 或 MinGW 修改其中的源码；编译适配只在外部副本或 `build/` 中做。
- `service/`：示例配置（`config_sample_*.conf` 是解析器的测试素材）与 systemd unit `service/kcptube`（不是二进制）。
- `docs/`：本项目的计划/进度；`legacy-cpp/docs/` 是上游使用文档，只读。
- 开发在 `rs-dev` 分支，`main` 保持与上游可对比。



## 依赖与代码约定

- 新依赖加在根 `Cargo.toml` 的 `[workspace.dependencies]`，crate 内写 `xxx.workspace = true`（现有项目的统一做法）。
- 改动加密/FEC 后必须保持 `crates/crypto/src/cipher.rs::vectors_match_botan` 通过；新增向量要注明由 C++ 版 Botan 生成。
- 非测试代码禁止 `unwrap()`/`expect()`；`unsafe` 默认禁止；错误用 `thiserror`；注释与文档用中文。
- 许可证：本项目以 `GPL-3.0-only` 分发，每个 `.rs` 文件首行固定为 `// SPDX-License-Identifier: GPL-3.0-only`；`legacy-cpp/` 保留上游 BSD 3-Clause 声明，**不要**改写其文件头或删除 `legacy-cpp/LICENSE`。新增第三方代码须确认与 GPLv3 兼容并登记到 `THIRD-PARTY.md`。
- 提交信息：`<类型>: <中文描述>`，类型用 `feat / fix / refactor / test / docs / ci / chore`。
- 里程碑有进展时同步更新 `docs/progress.md`。


