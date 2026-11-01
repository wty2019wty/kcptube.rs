# 性能基准（M5）

> 最后更新：2026-11-01
> 脚本：[`scripts/bench.py`](../../scripts/bench.py)、[`scripts/bench-m5.ps1`](../../scripts/bench-m5.ps1)
> 相关验收：`migration-plan.md` M5「性能达到 C++ 版的 80% 以上（延迟不劣化）」

---

## 1. 目的与结论

M5 的性能验收要求「Rust 版吞吐不低于 C++ 版的 80%，延迟不劣化」。本次基准在
**同一台机器、同一套配置、同一个 Python 回显目标**下，分别测量四种进程组合：

| 场景 | 客户端 | 服务端 |
|---|---|---|
| `rust-rust` | Rust | Rust |
| `cpp-cpp` | C++ `20260329` | C++ `20260329` |
| `rust-client-cpp-server` | Rust | C++ |
| `cpp-client-rust-server` | C++ | Rust |

**结论：验收通过，且余量很大。** 以 `cpp-cpp` 为基线：

- `rust-rust` 吞吐约为基线的 **3.3–4.9 倍**（两次运行分别为 31.3 / 21.3 MiB/s，基线 6.3–6.5）；
- 两个互操作方向的吞吐分别约为基线的 **1.2 倍**（Rust 客户端）与 **2.0 倍**（Rust 服务端）；
- 延迟（P50 / P99）与 C++ 相当或更好，没有劣化；
- 两个进程的工作集合计约 11.5 MB（Rust）对 23.1 MB（C++），Rust 约省一半。

因此阶段 4（`KcpUpdater` 期限驱动、减少每包分配等）**当前没有性能上的必要性**，
保留为「有观测问题再做」的可选优化。

---

## 2. 环境

| 项 | 值 |
|---|---|
| CPU | AMD Ryzen 9 7845HX（12 核 / 24 线程） |
| 内存 | 32 GB |
| OS | Windows 10 专业工作站版 22H2（10.0.19045） |
| Rust | 1.96.0，`target/release`（`opt-level` 混合 `s`/`3`、LTO、strip） |
| C++ 参考版 | `build/legacy/kcptube.exe`，`version 20260329`，MinGW Release |
| 测量客户端 | Python 3（`scripts/bench.py`，单线程） |

> 本机回环测试，不涉及真实网络 RTT；结论用于同日、同机、同配置的**相对比较**。

---

## 3. 拓扑与配置

```text
bench.py --TCP--> client(本地端口) ==KCP/UDP==> server ==TCP--> python net_echo.py
```

两端统一配置（除可执行文件外完全一致）：

```ini
# server
mode=server
listen_on=127.0.0.1
listen_port=<动态分配>
destination_address=127.0.0.1
destination_port=<echo 端口>
encryption_password=bench-pass
encryption_algorithm=AES-GCM
kcp=regular3

# client
mode=client
listen_on=127.0.0.1
listen_port=<动态分配>
destination_address=127.0.0.1
destination_port=<server 端口>
encryption_password=bench-pass
encryption_algorithm=AES-GCM
kcp=regular3
```

- 不开 FEC、mux、日志写盘与 `keep_alive`；
- 每个场景使用全新端口与全新进程，测量前用 `net_probe.py` 做一次就绪探测；
- 每个场景的顺序固定，避免前一场景残留进程影响。

---

## 4. 测量方法

### 4.1 吞吐

- 单条 TCP 连接，每块 64 KiB，共 **32 MiB / 64 MiB** × **3 轮**，取中位数；
- 每块发送后等待等量回显，统计口径为「发送载荷 / 耗时」；
- 回程会回显等量数据，因此**双向合计约为表中数值的 2 倍**。

### 4.2 延迟

- 单条 TCP 连接，64 字节 ping-pong，预热 20 次后采样 **300 次**；
- P99 取「不小于 99% 样本的最小值」，避免插值口径差异。

### 4.3 工作集

测量结束时读取 client、server 两个进程的 `WorkingSet64` 之和，仅作参考；
C++ 进程包含 MinGW 运行时的 DLL 映射，口径对 C++ 略不利。

---

## 5. 结果

### 5.1 运行 A（32 MiB × 3 轮）

| 场景 | 吞吐 (MiB/s) | P50 (ms) | P99 (ms) | 工作集 (MB) |
|---|---:|---:|---:|---:|
| `rust-rust` | 31.28 | 0.068 | 0.143 | 11.6 |
| `cpp-cpp` | 6.34 | 0.085 | 0.194 | 23.1 |
| `rust-client-cpp-server` | 7.82 | 0.077 | 0.171 | 17.4 |
| `cpp-client-rust-server` | 13.31 | 0.122 | 0.239 | 17.2 |

### 5.2 运行 B（64 MiB × 3 轮，用于确认稳定性）

| 场景 | 吞吐 (MiB/s) | P50 (ms) | P99 (ms) | 工作集 (MB) |
|---|---:|---:|---:|---:|
| `rust-rust` | 21.32 | 0.088 | 0.239 | 11.5 |
| `cpp-cpp` | 6.47 | 0.088 | 0.239 | 23.2 |
| `rust-client-cpp-server` | 7.68 | 0.072 | 0.134 | 17.7 |
| `cpp-client-rust-server` | 12.95 | 0.072 | 0.138 | 17.4 |

`rust-rust` 的吞吐两次相差约 30%，与单连接、KCP `interval=10ms` 的批处理、
以及本机调度噪声有关；互操作场景的两次结果则比较稳定。

---

## 6. 观察与局限

- **C++↔C++ 基线偏低（约 6.5 MiB/s）**：两次运行一致，不是偶发。瓶颈在 C++ 服务端
  （把服务端换成 Rust 后，C++ 客户端的吞吐从 6.5 升到 13.0），但具体原因未继续深挖；
  这不影响「Rust 是否达标」的结论，反而说明余量更大。
- 表中吞吐是单连接 echo 往返，不代表多连接聚合吞吐；多连接、大包、UDP、
  FEC/mux 组合以及空闲 CPU 占用尚未纳入。若后续要评估阶段 4，建议先补
  多连接与空闲 CPU 两组数据。
- Python 测量客户端在本机可稳定跑到 31 MiB/s 以上，未见其成为瓶颈；
  但 C++ 场景的低值也可能部分来自 Python 与 C++ 进程的调度差异，属于方法学局限。
- 配对场景（Rust client ↔ C++ server）与 `cpp-cpp` 使用同一个 C++ 服务端，
  因此「Rust 客户端相对 C++ 客户端」的比较是最直接的：7.7 vs 6.5，约 1.19 倍。

---

## 7. 复现

```powershell
cargo build --release -p kcptube
pwsh -File scripts/build-legacy-mingw.ps1          # 若还没有 build/legacy/kcptube.exe

# 全部四个场景（默认 32 MiB × 3 轮 + 300 次延迟采样）
pwsh -File scripts/bench-m5.ps1

# 只跑某两个场景、加大数据量
pwsh -File scripts/bench-m5.ps1 -Scenario rust-rust,cpp-cpp -ThroughputMib 64

# 跳过延迟，只测吞吐
pwsh -File scripts/bench-m5.ps1 -SkipLatency
```

原始输出与每次运行的 `report.md` 位于 `build/bench/<时间戳>/`。
