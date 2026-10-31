# 第三方代码与许可证

本项目整体以 **GNU General Public License v3.0 only**（`GPL-3.0-only`）分发，全文见 [`LICENSE`](LICENSE)。

下列代码并非本项目原创，随本项目一同分发时保留其原始许可证与版权声明。这些许可证均与 GPLv3 兼容，因此可以并入本项目，但**不得删除或改写其原始声明**。

## 1. `legacy-cpp/` — 上游 C++ 实现

| 项 | 内容 |
|---|---|
| 许可证 | BSD 3-Clause |
| 版权 | Copyright (c) 2022, cnbatch |
| 来源 | <https://github.com/cnbatch/kcptube> |
| 许可证全文 | [`legacy-cpp/LICENSE`](legacy-cpp/LICENSE) |

`legacy-cpp/` 是本项目的前身与协议基准，以只读参考实现的形式随仓库保留。除 GPL 要求的源码提供义务外，BSD 3-Clause 要求保留上述版权声明、条件列表与免责声明（第 1、2 条），并禁止使用版权人名义为衍生品背书（第 3 条）。本项目的 Rust 实现是对其协议行为的重新实现，**不属于** cnbatch 官方发布，也未获其背书。

## 2. `legacy-cpp/src/3rd_party/ikcp.*` — KCP 协议实现

| 项 | 内容 |
|---|---|
| 许可证 | MIT |
| 版权 | Copyright (c) 2017 Lin Wei (skywind3000) |
| 来源 | <https://github.com/skywind3000/kcp>（此处为经 cnbatch 修改的版本） |
| 许可证全文 | `legacy-cpp/src/3rd_party/KCP_LICENSE` |

`crates/kcp` 是对该实现的 Rust 移植，同样是 MIT 代码的衍生作品，随本项目以 GPLv3 分发。

## 3. Rust 依赖

`Cargo.lock` 中的第三方 crate 主要采用 MIT / Apache-2.0 双许可，二者均与 GPLv3 兼容，可安全并入本项目。新增依赖时请确认其许可证为 GPLv3 兼容（MIT、Apache-2.0、BSD、ISC、Zlib、Unlicense 等），**不得**引入 GPL 不兼容的许可证（如专有许可、CDDL、仅 GPLv2 等）。

建议用以下命令复核依赖许可证：

```bash
cargo install cargo-deny   # 一次性
cargo deny check licenses
```

## 4. 本项目原创代码

`crates/` 下的所有 Rust 源文件均为本项目原创（或由上述代码移植而来），以 GPLv3 授权，文件头统一标注：

```rust
// SPDX-License-Identifier: GPL-3.0-only
```

## 许可证变更说明

本项目早期版本曾以 BSD 3-Clause 发布，那些版本不受本次变更影响，仍可依其原许可证使用与再分发。自许可证变更为 GPLv3 之后的版本起，适用 GPLv3。
