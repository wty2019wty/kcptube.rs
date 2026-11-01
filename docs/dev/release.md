# 发布流程

> 最后更新：2026-11-01
> 相关脚本：[`scripts/build-release.ps1`](../../scripts/build-release.ps1)（仅本地手动打包）
> 工作流：[`.github/workflows/release.yml`](../../.github/workflows/release.yml)

---

## 1. 产物

| 产物 | 平台 | 说明 |
|---|---|---|
| `kcptube-<版本>-linux-x86.tar.gz` | Linux i686 | 静态链接（musl），无运行时依赖 |
| `kcptube-<版本>-linux-x64.tar.gz` | Linux x86_64 | 静态链接（musl），无运行时依赖 |
| `kcptube-<版本>-linux-arm.tar.gz` | Linux armv7（musleabihf） | 静态链接（musl），树莓派 32 位等 |
| `kcptube-<版本>-linux-aarch64.tar.gz` | Linux aarch64 | 静态链接（musl），无运行时依赖 |
| `kcptube-<版本>-windows-x86.zip` | Windows i686 | `kcptube.exe` + 文档 + 示例配置（静态 CRT） |
| `kcptube-<版本>-windows-x64.zip` | Windows x86_64 | `kcptube.exe` + 文档 + 示例配置（静态 CRT） |
| `SHA256SUMS.txt` | — | 上面全部产物的 SHA256 |

包内固定包含 `README.md`、`LICENSE`、`THIRD-PARTY.md`、`docs/*.md`（使用文档）、
`service/`（示例配置与 systemd unit）。不包含 `docs/dev/`。

版本号来自根 `Cargo.toml` 的 `[workspace.package] version`，发布 tag 约定为
`v<版本>`（例如 `v0.1.0`）。

---

## 2. GitHub Actions

`.github/workflows/release.yml` 在推送 `v*` tag 时触发，构建与打包逻辑全部内联在工作流里，
不调用 `scripts/build-release.ps1`：

1. **build**（6 个矩阵项）
   - Linux（`ubuntu-latest`）：用 [cross](https://github.com/cross-rs/cross) 构建
     `i686-unknown-linux-musl`、`x86_64-unknown-linux-musl`、`armv7-unknown-linux-musleabihf`、
     `aarch64-unknown-linux-musl`（Docker 交叉工具链，无需在本机装 zig）。
   - Windows（`windows-latest`）：用 msvc 工具链构建 `i686-pc-windows-msvc`、
     `x86_64-pc-windows-msvc`，并设 `RUSTFLAGS=-C target-feature=+crt-static`。
   - 每个矩阵项在工作流内完成暂存目录 + 打包（zip / tar.gz），再上传 artifact。
2. **publish**：下载全部产物，`sha256sum` 生成 `SHA256SUMS.txt`，用
   `softprops/action-gh-release` 创建 Release。

发布步骤：

```bash
git tag v0.1.0
git push origin v0.1.0
```

`workflow_dispatch` 可以手动跑 build，但不会创建 Release（publish 仅在 tag 上执行）。

---

## 3. 本地构建

本地脚本只覆盖 Windows x64 + Linux x64 两个目标，多架构发布以 CI 为准：

```powershell
# 两个平台都构建（Windows 上交叉编译 Linux 需要 zig）
$env:CARGO_ZIGBUILD_ZIG_PATH = "<zig.exe 路径>"
pwsh -File scripts/build-release.ps1

# 只构建当前平台
pwsh -File scripts/build-release.ps1 -Targets windows
```

- Linux 主机上脚本自动改用原生 `cargo build --target x86_64-unknown-linux-musl`，
  不会调用 zig。
- 产物写入 `dist/`（已加入 `.gitignore`）。

---

## 4. 发布前检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p kcptube -- --check-config service/config_sample_*.conf   # 只认输出行：No
```

- 确认根 `Cargo.toml` 的 `version` 与 tag 一致；
- 确认 CI 的 6 个 build 矩阵项全绿（含 `rust.yml` 的 `cross-check` 交叉编译检查）；
- 确认 Linux 产物是静态链接（`file`/`ldd` 应显示 musl 静态）；
- 下载 Release 产物后核对 `SHA256SUMS.txt`。
