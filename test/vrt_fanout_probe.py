"""Prove byte-identical LAN multicast data/context at two concurrent subscribers."""
import argparse
import hashlib
import json
import select
import socket
import sys
import time

from vrt_listener import decode


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--group", required=True)
    parser.add_argument("--port", type=int, default=4820)
    parser.add_argument("--interface", required=True)
    parser.add_argument("--source", required=True, help="expected collector IPv4 address")
    parser.add_argument("--seconds", type=float, default=10)
    args = parser.parse_args()
    sockets: list[socket.socket] = []
    seen: list[dict[tuple[int, int], dict[str, bytes]]] = [{}, {}]
    matched: dict[tuple[int, int], str] = {}
    counts = [0, 0]
    invalid = [0, 0]
    membership = socket.inet_aton(args.group) + socket.inet_aton(args.interface)
    deadline = time.monotonic() + args.seconds
    try:
        # Both subscribers have joined before any packet is consumed.
        for _ in range(2):
            sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP)
            sockets.append(sock)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 1024 * 1024)
            sock.bind(("", args.port))
            sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, membership)
            sock.setblocking(False)
        while time.monotonic() < deadline:
            ready, _, _ = select.select(sockets, [], [], min(0.2, max(0, deadline - time.monotonic())))
            for sock in ready:
                index = sockets.index(sock)
                try:
                    packet, source = sock.recvfrom(65535)
                except BlockingIOError:
                    continue
                if source[0] != args.source:
                    continue
                try:
                    info = decode(packet)
                except ValueError:
                    invalid[index] += 1
                    continue
                counts[index] += 1
                key = (info["stream_id"], info["type"])
                if key in matched:
                    continue
                digest = hashlib.sha256(packet).hexdigest()
                seen[index].setdefault(key, {})[digest] = packet
                if len(seen[index][key]) > 1024:
                    del seen[index][key][next(iter(seen[index][key]))]
                if seen[1 - index].get(key, {}).get(digest) == packet:
                    matched[key] = digest
                    print(json.dumps({"event": "identical_packet", "sha256": digest, **info}), flush=True)
            if any((stream, 1) in matched and (stream, 4) in matched for stream, _ in matched):
                break
    finally:
        for sock in sockets:
            sock.close()  # closes membership too, including error paths
    success = any((stream, 1) in matched and (stream, 4) in matched for stream, _ in matched)
    print(json.dumps({"event": "fanout_summary", "success": success, "runtime": sys.platform,
                      "group": args.group, "port": args.port, "interface": args.interface,
                      "source": args.source, "subscribers": 2, "received": counts,
                      "invalid": invalid}), flush=True)
    return 0 if success else 1


if __name__ == "__main__":
    raise SystemExit(main())
