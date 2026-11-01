# 自定义映射模式

> **说明**：Rust 版已完整接入自定义映射：客户端按映射在本地端口监听并在握手时把目标地址交给服务端，服务端忽略自己的 `destination_*` 配置；mux 模式通过 `pre_connect_custom_address` 提前下发目标。`--check-config` 与运行期行为均已与 C++ 版对齐。

在该模式下，可以自行设置映射规则。

安全起见，自定义映射模式并未默认开启，必须手动指定

## 使用方式

类似于原本的使用方式，唯一不同的是，客户端的监听设置、服务器端的目标指向，都设置成 `{}`。

客户端模式示例：
```
mode=client
kcp=regular3
inbound_bandwidth=500M
outbound_bandwidth=50M
listen_port={}
destination_port=3000
destination_address=123.45.67.89
encryption_password=qwerty1234
encryption_algorithm=AES-GCM
mux_tunnels=3

[custom_input]
127.0.0.1:13389 -> 123.45.67.89:3389

[custom_input_tcp]
:8000 -> 127.0.0.1:8000

[custom_input_udp]
:5000 -> [::1]:5000
```

服务端模式示例：
```
mode=server
kcp=regular3
inbound_bandwidth=1G
outbound_bandwidth=1G
listen_port=3000
destination_port={}
destination_address={}
encryption_password=qwerty1234
encryption_algorithm=AES-GCM
```

## 参数解析

#### 服务端
把 `destination_port` 或 `destination_address` 任意一个设为 `{}` 均可。最好两个都写。

#### 客户端
把 `listen_port` 或 `listen_on` 任意一个设为 `{}` 均可。

然后在各标签下设置地址映射。
- `[custom_input]` 表示该条目下的映射同时适用于 TCP 与 UDP。
- `[custom_input_tcp]` 表示该条目下的映射仅用于 TCP。
- `[custom_input_udp]` 表示该条目下的映射仅用于 UDP。

三个标签段可以只写其中一部分，段内为空也合法。

## 映射格式

映射格式以 `->` 做分隔（两侧可留空格）。

- 左侧为客户端监听地址及端口
    - 此处监听地址可以不填（如 `:8000`），表示监听所有可用接口
    - 监听端口必须填写
- 右侧为服务器端连接的地址与端口
    - IP 地址与端口都必须填写
    - 此处的 IP 表示服务器端连接的地址，例如填写 127.0.0.1，就表示服务器自己连接 127.0.0.1 （即服务器自身）。

IPv6 地址须写在 `[]` 内，例如：

```
[::1]:5000 -> [fe80::1]:80
```

## 解析规则

- 映射行必须写在 `[custom_input]` / `[custom_input_tcp]` / `[custom_input_udp]` 段内，否则报错：`invalid input: '...'. Does not belongs to any custom address sections.`（原文保留 C++ 版拼写）
- 端口必须是 1 ~ 65535；右侧地址不能为空，否则报 `Remote Address can't be empty`
- 同一个本地监听地址 + 端口重复出现时，后写的覆盖先写的
- 映射行优先于普通 `key=value` 行识别：只要一行里含有 `->`，就会按映射行解析
- 本地地址可以是 IP 或域名；IPv4 形状但非法的地址会报 `Address Incorrect`
