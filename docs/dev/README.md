# kcptube.rs 开发者文档

本目录只放**开发/维护相关文档**；面向使用者的配置与参数说明在上一级 [`docs/`](../README.md)。

> 使用文档与开发者文档分开放：`docs/` 面向「怎么用」，`docs/dev/` 面向「怎么改、改到哪了」。

## 文档索引

| 文档 | 内容 |
|---|---|
| [移植计划](migration-plan.md) | 目标、架构、阶段划分、协议关键事实、风险登记、文档维护规则 |
| [移植进度](progress.md) | 当前完成度、各模块状态、测试统计、已知问题与技术债、下一步、变更记录 |
| [优化与去重路线](refactor-backlog.md) | 跨模块重复审计（与 C++ 逐项对照）与阶段 1–5 的优化决策 |
| [relay 实施方案](relay-plan.md) | C++ `relay.cpp` 行为拆解、Rust 任务模型、对齐清单、R0–R5 实施记录 |
| [性能基准](benchmark.md) | M5 吞吐 / 延迟 / 内存对比（Rust vs C++），测量方法与结果 |

## 相关入口

- **使用文档首页**：[`docs/README.md`](../README.md)
- **参数手册**：[`docs/parameters.md`](../parameters.md)
- **兼容性约束与工程约定**：根目录 [`AGENTS.md`](../../AGENTS.md)、[`README.md`](../../README.md) 的「兼容性要点」
- **协议参考实现**：[`legacy-cpp/`](../../legacy-cpp/)（只读）
- **第三方代码清单**：[`THIRD-PARTY.md`](../../THIRD-PARTY.md)

## 维护约定

- 里程碑或行为有变化时同步 [`progress.md`](progress.md)（含第 5 节「已知问题」与第 7 节变更记录）。
- 参数语义变化同步 [`docs/parameters.md`](../parameters.md)。
- 有意偏离 C++ 行为的设计必须登记到 [`progress.md`](progress.md) 第 5 节。
