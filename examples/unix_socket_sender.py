#!/usr/bin/env python3

import json
import socket
import sys


def main() -> int:
    socket_path = sys.argv[1] if len(sys.argv) > 1 else "/run/iec104bridge/input.sock"
    message = {
        "ioa": 1001,
        "value": 132.4,
        "type": "float",
    }

    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.connect(socket_path)
        client.sendall((json.dumps(message) + "\n").encode("utf-8"))
        reply = client.recv(1024).decode("utf-8").strip()

    print(reply)
    return 0 if reply == "ok" else 1


if __name__ == "__main__":
    raise SystemExit(main())
