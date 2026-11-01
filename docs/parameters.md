# kcptube.rs 参数介绍

下表列出全部可用配置项。取值规则、默认值与校验行为均以 Rust 版代码为准（与 C++ 版配置解析逐项对齐，个别边界处理见文末[解析怪癖与兼容性说明](#解析怪癖与兼容性说明)）。

|  名称   | 可设置值  | 必填 |默认值|备注|
|  ----  | ----  | :----: | :----: | ---- |
| mode  | client<br>server<br>relay |是|无|客户端<br>服务端<br>中继节点。全文件只能出现一次|
| listen_on | IPv4 / IPv6 地址 |否|全部接口（IPv6 + IPv4 双栈）|只能填写 IPv4 或 IPv6 地址（不支持域名）。多个地址请用逗号分隔|
| listen_port | 1 - 65535 |是|无|以服务端运行时可以指定端口范围（如 `30000-30010`）|
| destination_port | 1 - 65535 |是|无|以客户端运行时可以指定端口范围|
| destination_address  | IP地址、域名 |是|无|填入 IPv6 地址时不需要中括号。多个地址请用逗号分隔|
| destination_dnstxt   | 域名               |否    |无|仅接受单个地址，仅限客户端使用。从 DNS TXT 记录当中获取 IP 地址及端口号。使用该参数时，不必填写 destination_address 及 destination_port。**当前版本尚未实现，仅解析不生效**|
| dport_refresh  | 0 - 32767 |否|60|单位「秒」。端口跳跃（动态端口）的刷新间隔。<br>1 至 19 按 20 秒算，大于 32767 按 32767 秒算。<br>设为 0 表示禁用端口跳跃。|
| encryption_algorithm | XOR<br>AES-GCM<br>AES-OCB<br>chacha20<br>xchacha20<br>none |否    |不加密|单纯异或运算<br>AES-256-GCM-AEAD<br>AES-256-OCB-AEAD<br>ChaCha20-Poly1305<br>XChaCha20-Poly1305<br>不加密|
| encryption_password  | 任意字符            |视情况|空|设置了 encryption_algorithm 使用加密时必填，none 与 XOR 除外|
| udp_timeout  | 0 - 65535 |否|180|单位「秒」。设为 0 或超出范围则使用预设值 180 秒<br>该选项表示的是，UDP 应用程序 ↔ kcptube 之间的超时设置|
| keep_alive  | 0 - 65535 |否 | 0|单位「秒」。预设值为 0，等于停用 Keep Alive<br>该选项是指两个 KCP 端之间的 Keep Alive<br>可单方面启用，用于检测通道是否停止响应。任何通道空闲超过 255 秒会被强制关闭|
| mux_tunnels  | 0 - 65535 |否 | 0|预设值为 0，等于不使用多路复用通道<br>该选项是指两个 KCP 端之间的多路复用通道数<br>**仅限客户端使用**，服务端 / 中继模式设置会报错|
| stun_server  | STUN 服务器地址 |否|无|listen_port 为端口范围模式时不可使用。**当前版本尚未实现，仅解析不生效**|
| update_ipv4          | 可执行文件的路径     |否    |无|用于保存 stun 获得的信息，请往后阅读。**当前版本尚未实现**|
| update_ipv6          | 可执行文件的路径     |否    |无|用于保存 stun 获得的信息，请往后阅读。**当前版本尚未实现**|
| log_path  | 存放 Log 的目录 |否|不写 Log|必须是已存在的目录，不能指向文件本身|
| fec  | uint8:uint8 |否|不使用|格式为 `fec=D:R`，例如可以填入 `fec=20:4`。<br>注意：D + R 的总数最大值为 255，不能超过这个数。<br>冒号两侧任意一个值为 0 表示不使用该选项。两端的设置必须相同。<br>详情请参考 [FEC使用介绍](fec.md)|
| mtu  | 正整数 |否|不设置|当前网络 MTU 数值，用以自动计算 kcp_mtu|
| kcp_mtu  | 正整数 |否|1418（启用 FEC 时 1405）|调用 ikcp_setmtu() 设置的值，亦即 UDP 数据包内数据内容的长度|
| kcp  | manual<br>fast1 - 6<br>regular1 - 5<br> &nbsp; |否|regular5|手动设置<br>快速<br>常速<br>(末尾数字：数值越小，速度越快)<br>不填写时按 regular5 处理|
| kcp_sndwnd  | 正整数 |否|见下表|可以单独覆盖预设值|
| kcp_rcvwnd  | 正整数 |否|见下表|可以单独覆盖预设值|
| kcp_nodelay  | 正整数 |视情况|见下表|kcp=manual 时必填，其余模式被预设值覆盖|
| kcp_interval  | 正整数 |视情况|见下表|kcp=manual 时必填，其余模式被预设值覆盖|
| kcp_resend  | 正整数 |视情况|见下表|kcp=manual 时必填，其余模式被预设值覆盖|
| kcp_nc  | yes<br>true<br>1<br>no<br>false<br>0 |视情况|见下表|kcp=manual 时必填，其余模式被预设值覆盖|
| outbound_bandwidth | 带单位后缀的正整数 |否|0（不限速）|出站带宽，用于通讯过程中动态更新 kcp_sndwnd 的值|
| inbound_bandwidth | 带单位后缀的正整数 |否|0（不限速）|入站带宽，用于通讯过程中动态更新 kcp_rcvwnd 的值|
| ipv4_only | yes<br>true<br>1<br>no<br>false<br>0 |否|不设置|若系统禁用了 IPv6，须启用该选项并设为 yes 或 true 或 1|
| ipv6_only | yes<br>true<br>1<br>no<br>false<br>0 |否|不设置|忽略 IPv4 地址|
| blast | yes<br>true<br>1<br>no<br>false<br>0 |否|**yes**|尝试忽略 KCP 流控设置，尽可能迅速地转发数据包。可能会导致负载过大|
| \[listener\] | N/A |是<br>(仅限中继模式)|N/A|中继模式的标签，用于指定监听模式的 KCP 设置<br>该标签表示与客户端交互数据|
| \[forwarder\] | N/A  |是<br>(仅限中继模式)|N/A|中继模式的标签，用于指定转运模式的 KCP 设置<br>该标签表示与服务端交互数据|
| \[custom_input\] | N/A  |否|N/A|自定义映射模式的标签，使用方法请参考 [自定义映射使用方法](custom-ip-mappings.md)（TCP / UDP / mux 均已接入）|
| \[custom_input_tcp\] | N/A  |否|N/A|自定义映射模式的标签，同上|
| \[custom_input_udp\] | N/A  |否|N/A|自定义映射模式的标签，同上|

其中，`encryption_algorithm` 以及 `encryption_password` 在通讯的两端必须保持一致。

## FreeBSD only
|  名称   | 可设置值  | 必填 |备注|
|  ----  | ----  | :----: | ---- |
| fib_ingress  | 0 - 65535 |否|入站连接使用的 FIB|
| fib_egress | 0 - 65535 |否|出站连接使用的 FIB|

**注意**：Rust 版当前可以解析这两个参数，但尚未应用到实际连接（SO_SETFIB）。

## outbound_bandwidth 与 inbound_bandwidth
可用后缀：K / k / M / m / G / g

后缀区分大小写，大写按二进制 (1024) 计算，小写按十进制 (1000) 计算。

- 填入 100k，表示带宽为 100 kbps (100000 bps)
- 填入 100K，表示带宽为 100 Kbps (102400 bps)
- 填入 100M，表示带宽为 100 Mbps (102400 Kbps)
- 填入 1G，表示带宽为 1 Gbps (1024 Mbps)

注意，是 bps (Bits Per Second)，不是 Bps (Bytes Per Second)。程序内部会按 ÷8 换算为字节/秒再使用。

**务必填写单位后缀**：解析时会无条件丢弃最后一个字符再转换数字（这是与 C++ 版一致的行为），且最终会按 ÷8 换算成字节/秒。因此纯数字写法会丢失个位数，甚至直接变成 0：例如 `64` 会被当成 `6` bps、再除 8 得 0，等于没有限速。请始终写成 `8K`、`100k`、`50M` 这类带单位的形式。

需要提醒的是，填写的带宽值不应超出实际带宽，以免造成发送窗口拥堵导致阻塞。

**重要提示**：<br>
KCPTube 会在 KCP 链路建立后，根据握手包的延迟值以及 outbound_bandwidth 与 inbound_bandwidth 的数值，计算并设置 KCP 的发送窗口大小。设置完成后的一段时间内，有很大机率出现流量大幅度波动的情况，甚至会出现流量突然降至 0，需要好几秒才能恢复。

两端建立连接时会**协商带宽**：实际生效的出站带宽 = min(本端 outbound_bandwidth，对端 inbound_bandwidth)。因此两端都应如实填写收发带宽。

## KCP 模式预设值

使用预设模式时，`kcp_nodelay`、`kcp_interval`、`kcp_resend`、`kcp_nc` 会被预设值**覆盖**（即使手写了这些参数）；`kcp_sndwnd` / `kcp_rcvwnd` 未填写时才取默认值，填写了则单独生效。

| 快速模式      | kcp_sndwnd | kcp_rcvwnd|kcp_nodelay|kcp_interval|kcp_resend|kcp_nc |
|  ----        | :----:     | :----:    | :----:    | :----:     | :----:   |:----: |
| fast1        | 2048       |   2048    |      1    |   0        |   2      |   1   |
| fast2        | 2048       |   2048    |      2    |   0        |   2      |   1   |
| fast3        | 2048       |   2048    |      1    |   1        |   3      |   1   |
| fast4        | 2048       |   2048    |      2    |   1        |   3      |   1   |
| fast5        | 2048       |   2048    |      1    |   1        |   4      |   1   |
| fast6        | 2048       |   2048    |      2    |   1        |   4      |   1   |

| 常速模式      | kcp_sndwnd | kcp_rcvwnd|kcp_nodelay|kcp_interval|kcp_resend|kcp_nc |
|  ----        | :----:     | :----:    | :----:    | :----:     | :----:   |:----: |
| regular1     | 1024       |   1024    |      1    |   1        |   5      |   1   |
| regular2     | 1024       |   1024    |      2    |   1        |   5      |   1   |
| regular3     | 1024       |   1024    |      0    |   1        |   2      |   1   |
| regular4     | 1024       |   1024    |      0    |   15       |   2      |   1   |
| regular5     | 1024       |   1024    |      0    |   30       |   2      |   1   |

> 注：未填写 `kcp=` 时按 regular5 处理。fast1 / fast2 的 `kcp_interval` 为 0（上游旧文档表格中误写为 1），本文档以代码实际预设值为准。
>
> `kcp=manual` 时必须自行填写 `kcp_nodelay`、`kcp_interval`、`kcp_resend`、`kcp_nc` 四项，缺任何一项都会报错（如 `kcp_nodelay not set`）；此时窗口默认值为 2048。

其中，丢包率越高（高于 10%），kcp_nodelay=1 就比 kcp_nodelay=2 越有优势。在丢包率不特别高的情况下，kcp_nodelay=2 可使延迟抖动更为平滑。

### 模式解读（简易版）
先说结论：模式的数字越靠前，响应速度越迅速。fast 模式稍有不同，请继续阅读。

`kcp_sndwnd` 指的是发送缓冲区的大小，`kcp_rcvwnd` 指的是接收缓冲区的大小。这两项会影响到收发速率。

`kcp_nodelay` 指的是 KCP 的 `nodelay` 变量，用于选择超时重传等待时长的增长速度。原本只有 0、1 之分，0 表示不使用 KCP 自身的快速重传，1 表示启用。<br />
- 数值为 0，则简单叠加，即新的时间是上一次时间的简单乘 2。<br />
- 数值为 1，表示新的等待时间仅为上一次时间乘 1.5，而不是2。<br />
- 2023 年 5 月，KCP 原作者又增加了数值 2，类似于数值 1。区别在于，数值 1 所用的"上一次时间"来自于当前数据包自己统计的数值，而数值 2 则使用 KCP 内部单独计算的平均延迟时间。<br />
这就是为什么前面提到过"kcp_nodelay=2 可使延迟抖动更为平滑"。

`kcp_interval` 指的是 KCP 内部更新间隔，`interval` 变量。

`kcp_resend` 指的是 KCP 内部的 `fastresend` 变量值，数值为 0 则表示关闭快速重传功能。若数值不为 0，表示跨越了指定次数后就不再等待，直接重传。

`kcp_nc` 指的是 `ikcp_nodelay()` 的最后一个参数 `nc`，0 表示不关闭流控， 1 表示关闭流控。此处应当设置成 1，否则传输速度会十分缓慢。

### 大流量传输
对于低丢包环境，每个模式都适合使用，区别只在于浪费的流量是多还是少，以及最高速的上限有所不同。其中 regular3 浪费的流量没那么多。<br />
建议同时开启 `blast=1` 设置。

对于高丢包环境，请考虑叠加使用 FEC 设置。详情请参考 [FEC使用介绍](fec.md)

## 加密与数据校验
由于需要传送 TCP 数据，因此数据校验是不可忽略的，正如 TCP 本身那样。

无论是否加密，kcptube 都会将 MTU 缩小 2 个字节，尾附 2 字节的数据。

如果已经使用了加密选项，那么尾附的 2 字节数据就是临时生成的 IV 原始值（用于还原 IV）。

如果选择不使用加密功能（包括不填写 `encryption_algorithm`），那么尾附的 2 字节数据就是校验码：CRC32 校验值折叠而成的 16 位校验码（高 16 位与低 16 位异或）。接收方会重新计算并比对，不一致即丢弃该包。

需要提醒的是，使用校验码仍然无法 100% 避免内容错误，TCP 本身也是一样。如果确实需要精确无误，请启用加密选项（AEAD 算法自带完整性校验）。

各算法的密钥与 IV 由 `encryption_password` 派生（SHA3），两端口令不一致时无法解出有效数据。

> 各模式的 IV 长度不同（AES-GCM 16 字节、AES-OCB 12 字节、chacha20 8 字节、xchacha20 24 字节），但对使用者透明，无需配置。

## 多路复用 (mux_tunnels=N)
KCP Tube 虽然有"多路复用"的功能，但默认并不主动打开。在不使用该功能的情况下，每接受一个入站连接，就会创建一个对应的出站连接。

原因是为了躲避运营商的 QoS。多路复用状态下，一旦某个端口号被 QoS，就会导致共用端口号的其它会话同时受阻，直到更换端口号为止。

连接之间相互独立，即使某个端口号被 QoS，受影响的仅仅只是这一路会话，不影响其他会话。

除非被承载的程序会产生大量独立连接。在这种情况下，KCP Tube 会创建大量 KCP 通道，在通讯过程中会消耗较多的 CPU 资源。

如果确实要用"多路复用"功能，可以参考以下分类：

- 适合使用"多路复用"的场景：
    - 代理转发程序，例如 Shadowsocks

- 不必使用"多路复用"的场景：
    - VPN，例如
        - OpenVPN
        - Wireguard

启用"多路复用"后，KCPTube 会预创建 N 条链路，所有入站新连接都会从已有链路中传送数据（轮询分配），而不再单独创建新链路。链路断开后会自动补建。

一般来说，`mux_tunnels` 设置成 3 ~ 10 就够用了，不需要设置过高的数值。

**注意**：`mux_tunnels` 只能在客户端配置中使用。服务端会根据客户端的连接方式自动适配单通道 / 多路复用，无需（也不允许）配置该参数。

## Log 文件

`log_path=` 必须指向目录，不能指向文件本身，且目录必须已存在。如果不写入 Log 文件，那就删除 `log_path` 这一行。

设置 `log_path` 后会写入以下文件（当前版本前两类已实现，`ip_address.log` 待 STUN/DDNS 接入后才有内容）：

| 文件 | 写入方式 | 内容 |
|---|---|---|
| `client_output.log` / `server_output.log` / `relay_output.log`（按模式取其一） | 追加 | 运行期事件：握手、端口跳跃、解密失败、会话回收等 |
| `<配置文件名>_status.log`（如 `client.conf_status.log`） | 覆盖 | 每 60 秒刷新一次的收发速率、FEC 恢复数、KCP 平均延迟摘要 |
| `ip_address.log` | 覆盖 | STUN / DDNS 获得的出口 IP 与端口（当前版本尚未实现，暂无内容） |

日志时间戳形如 `[2026-10-31 10:21:40 +0800]`。状态摘要也会同时输出到控制台。

**注意**：日志文件没有大小限制与滚动机制（messages 无限追加，status / ip 每次整体覆盖），与 C++ 版行为一致。如需长期运行，请配合 logrotate 等外部工具轮转。

### STUN 选项

这三个参数仅限服务器模式、中继模式时使用：
- stun_server
- update_ipv4
- update_ipv6

**注意**：Rust 版当前尚未实现 STUN / DDNS 功能，这三个参数可以写入配置并通过校验，但不会生效。以下为该功能的设计行为（与 C++ 版一致），供将来版本参考。

设置好 `update_ipv4` 或 `update_ipv6` 之后，程序会运行对应程序，把 stun 获取到的 IP 地址及端口传递过去。

例如，设置了
```
update_ipv4=/home/test/update_to_dnsv4
update_ipv6=/home/test/update_to_dnsv6
```

从 stun 获取到的地址、端口号分别是 130.131.132.133、23456，那么运行的文件及传递的参数是：

```
/home/test/update_to_dnsv4 130.131.132.133:23456
```
如果获取到的地址、端口号分别是 2409:ABCD:FEDC:3210::1、23456，那么运行的文件及传递的参数是：
```
/home/test/update_to_dnsv6 [2409:ABCD:FEDC:3210::1]:23456
```

另请注意：使用 STUN 打洞时，服务端无法侦听多端口，只能使用单端口模式，也不支持自定义侦听地址（配置校验会强制这一限制）。

# STUN Servers
从[NatTypeTeste](https://github.com/HMBSbige/NatTypeTester)找到的普通 STUN 服务器：
- stun.syncthing.net
- stun.qq.com
- stun.miwifi.com
- stun.bige0.com
- stun.stunprotocol.org

从[Natter](https://github.com/MikeWang000000/Natter)找到的STUN 服务器：

- fwa.lifesizecloud.com
- stun.isp.net.au
- stun.freeswitch.org
- stun.voip.blackberry.com
- stun.nextcloud.com
- stun.stunprotocol.org
- stun.sipnet.com
- stun.radiojar.com
- stun.sonetel.com
- stun.voipgate.com

其它 STUN 服务器：[public-stun-list.txt](https://gist.github.com/mondain/b0ec1cf5f60ae726202e)

## 忽略语法 `{}`

`listen_on`、`listen_port`、`destination_address`、`destination_port` 这四项可以写成 `{}`，表示「忽略该项」，配合[自定义映射](custom-ip-mappings.md)使用。各模式下的规则：

- **客户端**：`listen_on` 或 `listen_port` 任意一个设为 `{}` 即可清空监听设置，此时**必须**至少写一个 `[custom_input*]` 段，否则报 `custom address section tag is empty`；`destination_address` / `destination_port` 不允许忽略。
- **服务端**：`destination_address` 或 `destination_port` 任意一个设为 `{}` 即可忽略目标设置（改由客户端的映射指定），此时要求客户端在连接时提供目标地址；`listen_port` 不允许忽略（报 `listen_port can't be ignored`），`listen_on` 也不允许忽略（报 `if listen_address should be ignored, please delete the whole line of listen_address`）。

## 解析怪癖与兼容性说明

以下行为是与 C++ 版配置解析逐字对齐的结果，写配置时需要留意：

1. **只支持整行注释**，行尾注释会被当成值的一部分。
2. key 与匹配用的 value 不区分大小写；但 `encryption_password`、`log_path`、`listen_on`、`stun_server`、`update_ipv4`、`update_ipv6` 与带宽值**保留原始大小写**，而 `destination_address` / `destination_dnstxt` 会**转为小写**保存（照抄 C++ 行为）。
3. `key=`（空值）等同未设置，不会报错也不会清掉先前的值；重复 key 后写覆盖先写（多值列表是整体替换而非追加）；`mode=` 出现多于一次直接报错。
4. 数字解析允许尾部多余字符（`5099abc` 按 5099 解析），超出 int32 范围报 `invalid input: '...', stoi`。
5. 端口范围 `a-b` 含两端（`3000-3002` 是 3 个端口），要求 a < b；端口列表会去重、升序排列，末尾多余逗号会被容忍。
6. `udp_timeout` 写 0 **不会**关闭超时，而是回落到预设值 180 秒；关闭端口跳跃要用 `dport_refresh=0`。
7. `ipv4_only` / `ipv6_only` 只要出现就生效（不看值是不是 yes），两者同时出现时是否报错取决于书写的先后顺序（`ipv6_only` 在后则以 IPv6 为准，`ipv4_only` 在后才报 `Both ipv4_only and ipv6_only are set as true`）。这是 C++ 版的原有行为，Rust 版原样保留。
8. 未指定 `ipv4_only` / `ipv6_only` 时，监听通配地址为 `[::]` 并开启 IPv4 映射（**双栈**，IPv4 与 IPv6 客户端都能接入），与 C++ 版一致；只有 `ipv6_only=yes` 才是纯 IPv6 单栈。若系统禁用了 IPv6，请设置 `ipv4_only=yes` 退回 IPv4 单栈。
9. `fec=D:R` 中任意一侧为 0 表示关闭 FEC；若某侧为负数或超过 255，FEC 会被**静默整体关闭**（缺冒号、空侧则报错）。
10. 中继模式下，段外（根部）写下的公共配置会覆盖 `[listener]` / `[forwarder]` 内的对应值，详见[中继模式配置](relay-mode.md)。注意 `blast=no` 只能写在段外。
11. 双方系统时间的**绝对差**不得超过 **255 秒**（接收端按 `|本机时间 - 包时间戳| > 255` 丢弃），否则通讯异常。Linux/Windows 建议开启 NTP 自动校时；若一方时钟比另一方快或慢超过该阈值，握手阶段就会完全收不到对方的包。
12. 包格式没有版本号：任何协议改动都需要两端同时升级。
