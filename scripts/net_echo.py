"""TCP + UDP 回显服务器，供 T4/T10 验收脚本使用。

用法：python scripts/net_echo.py <port>
同一端口同时监听 TCP 与 UDP，收到的内容原样回送。
"""

import socket
import sys
import threading


def tcp_connection(conn: socket.socket) -> None:
    try:
        while True:
            data = conn.recv(65536)
            if not data:
                break
            conn.sendall(data)
    except OSError:
        pass
    finally:
        conn.close()


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: net_echo.py <port>", file=sys.stderr)
        return 2
    port = int(sys.argv[1])

    tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    tcp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    tcp.bind(("127.0.0.1", port))
    tcp.listen(64)

    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    udp.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    udp.bind(("127.0.0.1", port))

    def udp_loop() -> None:
        while True:
            try:
                data, peer = udp.recvfrom(65536)
                udp.sendto(data, peer)
            except OSError:
                return

    threading.Thread(target=udp_loop, daemon=True).start()
    print(f"echo listening on 127.0.0.1:{port} (tcp+udp)", flush=True)

    while True:
        conn, _ = tcp.accept()
        threading.Thread(target=tcp_connection, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    raise SystemExit(main())
