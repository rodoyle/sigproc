"""Bounded IPv4 multicast receiver/independent decoder for the capture profile.

No sigproc code is imported. Choose the actual Windows LAN address explicitly:
python test/vrt_listener.py --group 239.255.49.2 --port 4820 --interface 192.0.2.10
Success requires both CI16 data and matching context, not just group membership.
"""
import argparse
import ipaddress
import json
import socket
import struct
import time
from typing import NotRequired, TypedDict


class PacketInfo(TypedDict):
    type: int
    stream_id: int
    sequence: int
    seconds: int
    fractional_samples: int
    complex_samples: NotRequired[int]
    first_iq: NotRequired[tuple[int, int]]
    bandwidth_hz: NotRequired[float]
    frequency_hz: NotRequired[float]
    gain_db: NotRequired[float]
    sample_rate_hz: NotRequired[float]


def decode(packet: bytes) -> PacketInfo:
    if len(packet) < 20 or len(packet) % 4:
        raise ValueError("truncated VRT words")
    header, stream_id, seconds, frac_hi, frac_lo = struct.unpack_from("!5I", packet)
    if (header & 0xFFFF) * 4 != len(packet):
        raise ValueError("total VRT word count disagrees with datagram")
    if header & 0x0F000000 or (header >> 20) & 0xFF != 0x0D:
        raise ValueError("not the capture timestamp/no-class/no-trailer profile")
    result: PacketInfo = {
        "type": header >> 28, "stream_id": stream_id,
        "sequence": (header >> 16) & 15, "seconds": seconds,
        "fractional_samples": (frac_hi << 32) | frac_lo,
    }
    if result["type"] == 1:
        result["complex_samples"] = (len(packet) - 20) // 4
        if result["complex_samples"] == 0:
            raise ValueError("empty sample packet")
        result["first_iq"] = struct.unpack_from("!hh", packet, 20)
    elif result["type"] == 4:
        if len(packet) != 60:
            raise ValueError("context must contain 15 words")
        cif0 = struct.unpack_from("!I", packet, 20)[0]
        if cif0 & 0x7FFFFFFF != 0x28A08000:
            raise ValueError("unexpected context fields")
        result.update(
            bandwidth_hz=struct.unpack_from("!q", packet, 24)[0] / 2**20,
            frequency_hz=struct.unpack_from("!q", packet, 32)[0] / 2**20,
            gain_db=struct.unpack_from("!h", packet, 42)[0] / 128,
            sample_rate_hz=struct.unpack_from("!q", packet, 44)[0] / 2**20,
        )
        if struct.unpack_from("!Q", packet, 52)[0] != 0x200003CF00000000:
            raise ValueError("context does not announce CI16")
    else:
        raise ValueError("not data or context")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--group", required=True)
    parser.add_argument("--port", type=int, default=4820)
    parser.add_argument("--interface", required=True, help="local IPv4 LAN address")
    parser.add_argument("--seconds", type=float, default=10)
    parser.add_argument("--packets", type=int, default=8, help="minimum data packets before success")
    args = parser.parse_args()
    if not ipaddress.IPv4Address(args.group).is_multicast:
        parser.error("group must be IPv4 multicast")
    ipaddress.IPv4Address(args.interface)
    if args.seconds <= 0 or args.packets < 1 or not 1 <= args.port <= 65535:
        parser.error("positive seconds/packets and valid UDP port required")
    membership = socket.inet_aton(args.group) + socket.inet_aton(args.interface)
    counts, first, sources, invalid = {}, {}, set(), 0
    deadline = time.monotonic() + args.seconds
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_UDP) as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("", args.port))
        sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, membership)
        print(json.dumps({"event": "joined", "group": args.group, "port": args.port,
                          "interface": args.interface}), flush=True)
        try:
            while time.monotonic() < deadline:
                sock.settimeout(min(0.5, max(0.001, deadline - time.monotonic())))
                try:
                    packet, source = sock.recvfrom(65535)
                except TimeoutError:
                    continue
                try:
                    parsed = decode(packet)
                except ValueError:
                    invalid += 1
                    continue
                key = (parsed["stream_id"], parsed["type"])
                counts[key] = counts.get(key, 0) + 1
                sources.add(source)
                if key not in first:
                    first[key] = parsed
                    print(json.dumps({"event": "first_packet", "source": source, **parsed}), flush=True)
                if any(counts.get((stream, 1), 0) >= args.packets
                       and counts.get((stream, 4), 0) > 0 for stream, _ in counts):
                    break
        finally:
            sock.setsockopt(socket.IPPROTO_IP, socket.IP_DROP_MEMBERSHIP, membership)
    success = any(counts.get((stream, 1), 0) >= args.packets
                  and counts.get((stream, 4), 0) > 0 for stream, _ in counts)
    print(json.dumps({"event": "summary", "success": success, "sources": sorted(sources),
                      "counts": {f"{stream}:{kind}": count for (stream, kind), count in counts.items()},
                      "invalid": invalid}), flush=True)
    return 0 if success else 1


if __name__ == "__main__":
    raise SystemExit(main())
