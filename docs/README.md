# kcptube.rs 使用文档

[KCP Tube](https://github.com/cnbatch/kcptube) 的 Rust 重写版，与 C++ 版**线格式 1:1 兼容**（对齐 `20260329` 版本），可以与现有的 kcptube 客户端 / 服务端混跑：一端用 Rust 版、另一端用 C++ 版均可正常通讯。

> 本仓库是社区重写项目，非上游官方。原 C++ 实现完整保留在 [`legacy-cpp/`](../legacy-cpp/)，作为协议参考与互操作测试的基准。本文档基于 Rust 版代码的实际行为编写，与 [`legacy-cpp/docs/`](../legacy-cpp/docs/) 的上游文档可能存在细节差异，**请以本文档为准**。

## 当前实现状态

| 功能 | 状态 |
|---|---|
| 客户端模式 / 服务端模式（TCP + UDP 转发） | 🟢 可用 |
| 端口跳跃（动态端口，`dport_refresh`） | 🟢 可用 |
| FEC 前向纠错（`fec=D:R`） | 🟢 可用 |
| mux 多路复用（`mux_tunnels`，仅客户端） | 🟢 可用 |
| 保活（`keep_alive`）、带宽限速、`blast` | 🟢 可用 |
| 日志（`log_path`） | 🟢 可用 |
| `--check-config` 配置校验 | 🟢 可用 |
| `--try` 连通性测试 | 🟡 尚未实现（运行时仅提示） |
| 中继模式（`mode=relay`） | 🟡 配置可解析，运行时尚未实现 |
| 自定义映射（`[custom_input*]`） | 🟡 配置可解析，转发尚未接入 |
| STUN / DDNS（`stun_server`、`update_ipv4`、`update_ipv6`、`destination_dnstxt`） | 🟡 尚未实现（参数可解析、可校验） |
| FreeBSD 策略路由（`fib_ingress` / `fib_egress`） | 🟡 尚未实现（参数可解析） |

## 模式介绍

### 支持的模式
目前支持 3 种模式：
- 客户端模式
- 服务端模式
- 中继节点模式（当前版本尚不可用，仅配置校验可用）

## 用法

### 命令行
`kcptube config.conf`
或
`kcptube config1.conf config2.conf`
如果有多个文件，可以接着补充。多个配置文件会各自独立解析并同时运行。

#### 验证配置文件
使用 ``--check-config`` 选项即可验证配置文件是否正确：
```
kcptube --check-config config1.conf
```
或
```
kcptube config1.conf --check-config
```

验证时会逐个检查配置文件，输出形如：

```text
kcptube version 0.1.0
Error(s) found in setting file config1.conf
        listen_port is not set

Error Found in Configuration File(s): Yes
```

只要任意配置文件存在错误，程序就不会启动任何模式（无论是否指定 ``--check-config``）。

#### 连通性测试（尚未实现）
如果想在连接前测试一下连接是否畅通，可以加上 ``--try`` 选项：

```
kcptube --try config1.conf
```
或
```
kcptube config1.conf --try
```

**注意**：Rust 版当前尚未实现连通性测试，指定 ``--try`` 会打印「连通性测试（--try）尚未实现」后直接退出。该功能计划在后续版本补齐。

#### 版本号
Rust 版只对外自报一个版本号，来源是根 `Cargo.toml` 里声明的 `version`：

- 运行横幅、usage 与 `--version` 三处输出一致（当前为 `kcptube version 0.1.0`）；
- 该号码是 Rust 重写版自身的发布版本，**不是**对 C++ 版的协议兼容目标版本；
- 兼容目标（`20260329`）记录在 `docs/migration-plan.md`，不再通过横幅显示，因此与 C++ 版横幅不再逐字一致。

#### 客户端与服务端模式示例
请前往[常规配置](client-server.md)介绍页面

#### 中继节点模式示例
请前往[中继模式配置](relay-mode.md)介绍页面

## 参数介绍
请前往[参数列表](parameters.md)

## 配置文件格式速览

- 配置文件是平铺的 `key=value` 格式，一行一个配置项；中继模式与自定义映射使用 `[listener]`、`[forwarder]`、`[custom_input]` 等段标签划分区域。
- **只支持整行注释**：以 `#` 开头的整行会被跳过。**不支持行尾注释**，`key=value # 说明` 会把 `# 说明` 当作值的一部分。
- key 与大部分 value **不区分大小写**；但密码、路径、`listen_on` 的值会原样保留。
- `key=`（值留空）等同于未设置。同一个 key 重复出现时，后者覆盖前者；但 `mode=` 只能出现一次，出现多次会报错。
- 数值解析兼容 C++ `std::stoi` 的行为：`5099abc` 会按 5099 解析（允许尾部多余字符）。

更详细的取值规则见[参数列表](parameters.md)。

## 文档导航

| 文档 | 内容 |
|---|---|
| [参数列表](parameters.md) | 全部配置项、KCP 预设值、加密与校验、带宽、日志 |
| [常规配置](client-server.md) | 客户端 / 服务端基础配置、动态端口、多地址、手动 KCP |
| [中继模式配置](relay-mode.md) | 中继节点配置（当前版本尚未实现运行期） |
| [FEC 使用介绍](fec.md) | 前向纠错的格式、调优建议 |
| [MTU 设置](mtu.md) | `mtu` / `kcp_mtu` 的换算与开销 |
| [自定义映射](custom-ip-mappings.md) | `[custom_input*]` 映射规则（当前版本尚未接入转发） |
| [具体示例](specific-examples.md) | OpenVPN、HTTP、Shadowsocks、游戏等场景示例 |

移植计划、进度与已知问题见开发者文档：[移植计划](migration-plan.md)、[移植进度](progress.md)。

## 配置文件生成器

若需要配置文件生成器，请前往此处：[KCPTube Generator](https://github.com/cnbatch/KCPTubeGenerator)
（生成器面向 C++ 版设计，生成的配置文件同样适用于 Rust 版；尚未实现的参数会被 Rust 版解析但暂不生效。）
