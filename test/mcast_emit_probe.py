"""Emit VRT-shaped multicast datagrams from ONE pinned interface, at a fixed rate.

This is the send-side half of a duplicate investigation. `vrt_path_probe.py`
counts copies arriving at a receiver; this emits a known number so "2.0 copies
per packet" can be attributed to the emit path, the host's egress, or the L2.

The interesting knob is `--loop`. Linux's IP_MULTICAST_LOOP makes the kernel
deliver the sender its own datagram locally; it must NOT put a second frame on
the wire. A receiver on a different host that counts more than one copy per
sequence number therefore proves the duplication is below the application.

    # Emit 5000 datagrams at 5.7k/s from the LAN address, no local echo.
    python test/mcast_emit_probe.py --group 239.255.49.2 --port 4821 \
        --interface 192.0.2.10 --count 5000 --rate 5772 --loop off

Prints a JSON summary of what it actually emitted so a receiver's count can be
compared against a known denominator.
"""
import argparse
import ipaddress
import json
import socket
import struct
import time

# Same VRT profile as the capture service: type-1 IF Data, 15-bit word count,
# stream id, integer seconds + 64-bit fractional sample count, then IQ pairs.
STREAM_ID = 0x0000_0001
SAMPLES = 9  # 9 complex samples -> 20 byte header + 36 bytes payload


def data_packet(sequence: int) -> bytes:
    counter = sequence * 1000
    payload = struct.pack(f"!{SAMPLES * 2}h", *([0] * (SAMPLES * 2)))
    words = (20 + len(payload)) // 4
    # VRT header: type 1 in bits 31:28, total packet size in words in bits 15:0.
    header = (0x1 << 28) | (words & 0xFFFF)
    return (
        struct.pack("!5I", header, STREAM_ID, counter >> 32, counter & 0xFFFF_FFFF, 0)
        + payload
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--group", required=True)
    parser.add_argument("--port", type=int, default=4821)
    parser.add_argument("--interface", required=True, help="local IPv4 egress address")
    parser.add_argument("--count", type=int, default=5000)
    parser.add_argument("--rate", type=float, default=5772.0, help="datagrams per second")
    parser.add_argument("--ttl", type=int, default=1)
    parser.add_argument("--loop", choices=["on", "off"], default="off")
    args = parser.parse_args()
    if not ipaddress.IPv4Address(args.group).is_multicast:
        parser.error("group must be IPv4 multicast")
    egress = ipaddress.IPv4Address(args.interface)
    if args.count < 1 or args.rate <= 0:
        parser.error("positive count and rate required")

    payloads = [data_packet(i) for i in range(args.count)]
    interval = 1.0 / args.rate
    sent = 0
    mismatched = 0
    started = time.monotonic()
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP) as sock:
        sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(args.interface))
        sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, args.ttl)
        sock.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_LOOP, 1 if args.loop == "on" else 0)
        print(json.dumps({"event": "emitting", "group": args.group, "port": args.port,
                          "interface": str(egress), "count": args.count, "rate": args.rate,
                          "ttl": args.ttl, "loop": args.loop}), flush=True)
        target = started
        for payload in payloads:
            written = sock.sendto(payload, (args.group, args.port))
            if written == len(payload):
                sent += 1
            else:
                mismatched += 1
            # Absolute schedule so a slow send cannot compound into a rate drift.
            target += interval
            delay = target - time.monotonic()
            if delay > 0:
                time.sleep(delay)
    elapsed = time.monotonic() - started
    print(json.dumps({"event": "summary", "emitted": sent, "short_writes": mismatched,
                      "seconds": round(elapsed, 3),
                      "actual_rate": round(sent / elapsed, 1) if elapsed else None}), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
