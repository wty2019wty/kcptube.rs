"""经 kcptube 客户端端口做 TCP / UDP 回显探测。

用法：
  python scripts/net_probe.py tcp --port 5099 --text hello --count 3
  python scripts/net_probe.py udp --port 5099 --text hello --count 3
  python scripts/net_probe.py tcp --port 5099 --size 32768 --count 1
"""

import argparse
import socket
import sys


def build_payload(args: argparse.Namespace) -> bytes:
    if args.size > 0:
        seed = (args.text or "kcptube").encode()
        return (seed * (args.size // len(seed) + 1))[: args.size]
    return (args.text or "kcptube").encode()


def probe_tcp(host: str, port: int, payload: bytes, count: int, timeout: float) -> None:
    with socket.create_connection((host, port), timeout=timeout) as conn:
        for index in range(count):
            conn.sendall(payload)
            received = b""
            while len(received) < len(payload):
                chunk = conn.recv(min(65536, len(payload) - len(received)))
                if not chunk:
                    raise RuntimeError(f"TCP 第 {index} 条：连接提前关闭")
                received += chunk
            if received != payload:
                raise RuntimeError(
                    f"TCP 第 {index} 条：回显不一致（发送 {len(payload)}，收到 {len(received)}）"
                )


def probe_udp(host: str, port: int, payload: bytes, count: int, timeout: float) -> None:
    conn = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    conn.settimeout(timeout)
    try:
        for index in range(count):
            conn.sendto(payload, (host, port))
            received, _ = conn.recvfrom(65536)
            if received != payload:
                raise RuntimeError(
                    f"UDP 第 {index} 条：回显不一致（发送 {len(payload)}，收到 {len(received)}）"
                )
    finally:
        conn.close()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["tcp", "udp"])
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--text", default="kcptube")
    parser.add_argument("--size", type=int, default=0, help="按指定字节数发送（覆盖 text 长度）")
    parser.add_argument("--count", type=int, default=3)
    parser.add_argument("--timeout", type=float, default=10.0)
    args = parser.parse_args()

    payload = build_payload(args)
    try:
        if args.mode == "tcp":
            probe_tcp(args.host, args.port, payload, args.count, args.timeout)
        else:
            probe_udp(args.host, args.port, payload, args.count, args.timeout)
    except (OSError, RuntimeError) as error:
        print(f"PROBE FAIL: {error}", file=sys.stderr)
        return 1

    print(
        f"PROBE OK: {args.mode} {args.host}:{args.port} "
        f"count={args.count} bytes={len(payload)}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
