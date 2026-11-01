# 发布流程

> 最后更新：2026-11-01
> 相关脚本：[`scripts/build-release.ps1`](../../scripts/build-release.ps1)
> 工作流：[`.github/workflows/release.yml`](../../.github/workflows/release.yml)

---

## 1. 产物

| 产物 | 平台 | 说明 |
|---|---|---|
| `kcptube-<版本>-windows-x64.zip` | Windows x86_64 | `kcptube.exe` + 文档 + 示例配置 |
| `kcptube-<版本>-linux-x64.tar.gz` | Linux x86_64 | 静态链接（musl），无运行时依赖 |
| `SHA256SUMS.txt` | — | 上面两个文件的 SHA256 |

包内固定包含 `README.md`、`LICENSE`、`THIRD-PARTY.md`、`docs/*.md`（使用文档）、
`service/`（示例配置与 systemd unit）。不包含 `docs/dev/`。

版本号来自根 `Cargo.toml` 的 `[workspace.package] version`，发布 tag 约定为
`v<版本>`（例如 `v0.1.0`）。

---

## 2. GitHub Actions

`.github/workflows/release.yml` 在推送 `v*` tag 时触发：

1. **build**（矩阵）
   - `ubuntu-latest`：`apt-get install musl-tools` + `rustup target add x86_64-unknown-linux-musl`，
     然后调用 `scripts/build-release.ps1 -Targets linux`。**不需要 zig**。
   - `windows-latest`：调用 `scripts/build-release.ps1 -Targets windows`（msvc 工具链）。
   - 两个 job 各自上传 `dist/kcptube-*` 产物。
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

```powershell
# 两个平台都构建（Windows 上交叉编译 Linux 需要 zig）
$env:CARGO_ZIGBUILD_ZIG_PATH = "<zig.exe 路径>"
pwsh -File scripts/build-release.ps1

# 只构建当前平台
pwsh -File scripts/build-release.ps1 -Targets windows
```

- Linux 主机上脚本自动改用原生 `cargo build --target x86_64-unknown-linux-musl`，
  不会调用 zig（与 CI 一致）。
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
- 确认 Linux 产物是静态链接（CI 日志里 `musl-tools` 安装成功、`--target musl` 生效）；
- 下载 Release 产物后核对 `SHA256SUMS.txt`。
