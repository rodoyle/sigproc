"""Count VITA49 copies per transport path and byte-compare them.

The consumer drops the second copy of every datagram, so its own counters cannot
say WHICH path delivered the extra copies. This probe answers that directly: it
counts datagrams per sender address and, for every (stream, counter) it saw more
than once, compares the two payloads byte for byte.

Source ports separate the paths. The capture service sends the unicast copy and
the multicast copy from two different sockets, so a duplicate rate above ~50 %
shows up here as one sender address counting ~2x the other.

    python test/vrt_path_probe.py --group 239.255.49.2 --port 4820 \
        --interface 192.0.2.10 --seconds 10

Exit code is 0 when the probe observed something interpretable, not when the
paths agree: a mismatch is reported and still exits 0, because this is a
diagnostic, not a gate. Use --strict to make a mismatch or an unexplained extra
copy a failure.
"""
import argparse
import contextlib
import ipaddress
import json
import socket
import struct
import sys
import time
from collections import defaultdict
from typing import TypedDict


class PacketKey(TypedDict):
    stream_id: int
    kind: int
    counter: int


def key_of(packet: bytes) -> PacketKey:
    """(stream, VRT type, sample counter) for one datagram."""
    if len(packet) < 20 or len(packet) % 4:
        raise ValueError("truncated VRT words")
    header, stream_id, seconds, frac_hi, frac_lo = struct.unpack_from("!5I", packet)
    if (header & 0xFFFF) * 4 != len(packet):
        raise ValueError("total VRT word count disagrees with datagram")
    return {
        "stream_id": stream_id,
        "kind": header >> 28,
        "counter": (seconds << 64) | (frac_hi << 32) | frac_lo,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--group", required=True)
    parser.add_argument("--port", type=int, default=4820)
    parser.add_argument("--interface", required=True, help="local IPv4 LAN address")
    parser.add_argument("--seconds", type=float, default=10)
    parser.add_argument("--strict", action="store_true",
                        help="fail on a mismatch or an unexplained extra copy")
    args = parser.parse_args()
    if not ipaddress.IPv4Address(args.group).is_multicast:
        parser.error("group must be IPv4 multicast")
    ipaddress.IPv4Address(args.interface)
    if args.seconds <= 0 or not 1 <= args.port <= 65535:
        parser.error("positive seconds and a valid UDP port required")

    membership = socket.inet_aton(args.group) + socket.inet_aton(args.interface)
    per_source: defaultdict[str, int] = defaultdict(int)
    per_key: defaultdict[tuple, list[tuple[str, bytes]]] = defaultdict(list)
    invalid = 0

    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP) as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        # A large buffer keeps this probe from dropping copies, which would
        # understate the very duplication it is here to measure.
        with contextlib.suppress(OSError):
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 8 << 20)
        sock.bind(("", args.port))
        sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, membership)
        print(json.dumps({"event": "listening", "group": args.group, "port": args.port,
                          "interface": args.interface, "seconds": args.seconds}),
              flush=True)
        deadline = time.monotonic() + args.seconds
        try:
            while time.monotonic() < deadline:
                sock.settimeout(min(0.5, max(0.001, deadline - time.monotonic())))
                try:
                    packet, source = sock.recvfrom(65535)
                except TimeoutError:
                    continue
                label = f"{source[0]}:{source[1]}"
                per_source[label] += 1
                try:
                    key = key_of(packet)
                except ValueError:
                    invalid += 1
                    continue
                # Keep EVERY copy, including two from one source: a sender that
                # transmits the same datagram twice has the same source address
                # both times, and collapsing those would hide the duplication
                # this probe exists to measure.
                per_key[(key["stream_id"], key["kind"], key["counter"])].append((label, packet))
        finally:
            sock.setsockopt(socket.IPPROTO_IP, socket.IP_DROP_MEMBERSHIP, membership)

    copies = defaultdict(int)
    mismatches = []
    compared = 0
    for key, bucket in per_key.items():
        copies[len(bucket)] += 1
        if len(bucket) < 2:
            continue
        compared += 1
        reference_label, reference = bucket[0]
        for other_label, other in bucket[1:]:
            if other != reference:
                mismatches.append({"key": list(key), "a": reference_label, "b": other_label})

    summary = {
        "event": "summary",
        "per_source": dict(sorted(per_source.items(), key=lambda kv: -kv[1])),
        "distinct_packets": len(per_key),
        "copies_per_packet": {str(n): c for n, c in sorted(copies.items())},
        "packets_seen_on_multiple_paths": compared,
        "byte_mismatches": len(mismatches),
        "mismatch_examples": mismatches[:5],
        "invalid": invalid,
    }
    print(json.dumps(summary), flush=True)

    if args.strict:
        if mismatches:
            print("FAIL: copies of the same packet differ between paths", file=sys.stderr)
            return 1
        extra = sum(count for n, count in copies.items() if n > 2)
        if extra:
            print(f"FAIL: {extra} packets arrived on more than two paths", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
