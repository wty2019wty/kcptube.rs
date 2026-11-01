"""kcptube 性能基准的测量客户端（供 scripts/bench-m5.ps1 调用）。

对 kcptube 客户端的本地监听端口做 TCP echo 往返，分别测量：

- throughput：固定总字节数的往返吞吐（发送后等待同样长度的回显），MiB/s；
- latency：固定小包的 ping-pong 往返延迟，输出 P50 / P99（毫秒）。

两者都通过同一条已经建好的 TCP 连接完成，避免握手开销混入测量。
用法见 `python scripts/bench.py --help`。
"""

import argparse
import socket
import statistics
import sys
import time


def recv_exact(conn: socket.socket, size: int) -> None:
    """收满 `size` 字节，连接提前关闭时抛错。"""
    remaining = size
    while remaining > 0:
        chunk = conn.recv(min(65536, remaining))
        if not chunk:
            raise RuntimeError("连接提前关闭")
        remaining -= len(chunk)


def connect(host: str, port: int, timeout: float) -> socket.socket:
    conn = socket.create_connection((host, port), timeout=timeout)
    conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return conn


def bench_throughput(
    host: str, port: int, chunk: int, total_bytes: int, timeout: float
) -> float:
    payload = bytes((index * 31 + 7) & 0xFF for index in range(chunk))
    with connect(host, port, timeout) as conn:
        # 先做一次小往返，确认会话已经建立（首次连接可能仍在握手）。
        conn.sendall(b"warmup")
        recv_exact(conn, 6)

        start = time.perf_counter()
        remaining = total_bytes
        while remaining > 0:
            size = min(chunk, remaining)
            conn.sendall(payload[:size])
            recv_exact(conn, size)
            remaining -= size
        elapsed = time.perf_counter() - start

    if elapsed <= 0:
        raise RuntimeError("计时异常")
    return total_bytes / (1024.0 * 1024.0) / elapsed


def bench_latency(
    host: str, port: int, size: int, count: int, warmup: int, timeout: float
) -> tuple[float, float, float]:
    payload = b"k" * size
    samples: list[float] = []
    with connect(host, port, timeout) as conn:
        conn.sendall(b"warmup")
        recv_exact(conn, 6)
        for _ in range(warmup):
            conn.sendall(payload)
            recv_exact(conn, size)

        for _ in range(count):
            start = time.perf_counter_ns()
            conn.sendall(payload)
            recv_exact(conn, size)
            samples.append((time.perf_counter_ns() - start) / 1_000_000.0)

    samples.sort()
    p50 = statistics.median(samples)
    # 与常见 P99 口径一致：取不小于 99% 样本的最小值。
    index = max(0, min(len(samples) - 1, int(round(0.99 * len(samples))) - 1))
    p99 = samples[index]
    return p50, p99, sum(samples) / len(samples)


def main() -> int:
    parser = argparse.ArgumentParser(description="kcptube TCP echo 基准测量客户端")
    parser.add_argument("metric", choices=["throughput", "latency"])
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--chunk", type=int, default=32768, help="吞吐模式每次往返的字节数")
    parser.add_argument("--size-mib", type=float, default=8.0, help="吞吐模式的总字节数（MiB）")
    parser.add_argument("--size", type=int, default=64, help="延迟模式的包长（字节）")
    parser.add_argument("--count", type=int, default=300, help="延迟模式的往返次数")
    parser.add_argument("--warmup", type=int, default=20, help="延迟模式的预热次数")
    parser.add_argument("--timeout", type=float, default=30.0)
    args = parser.parse_args()

    try:
        if args.metric == "throughput":
            total = int(args.size_mib * 1024 * 1024)
            mib_s = bench_throughput(args.host, args.port, args.chunk, total, args.timeout)
            print(f"BENCH throughput mib_s={mib_s:.2f} bytes={total}")
        else:
            p50, p99, mean = bench_latency(
                args.host, args.port, args.size, args.count, args.warmup, args.timeout
            )
            print(
                f"BENCH latency ms_p50={p50:.3f} ms_p99={p99:.3f} ms_mean={mean:.3f} "
                f"count={args.count} size={args.size}"
            )
    except (OSError, RuntimeError) as error:
        print(f"BENCH FAIL: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
