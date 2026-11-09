#!/usr/bin/env python3
"""Set the capture sample rate using its bounded VITA 49.2 control profile."""
import argparse
import math
import secrets
import socket
import struct
import sys
import time

SCALE = 1 << 20


def request(stream_id, rate, message_id):
    if not math.isfinite(rate) or not 0 < rate <= ((1 << 63) - 1) / SCALE:
        raise ValueError("sample rate must be finite, positive and representable in VRT Q20")
    encoded = round(rate * SCALE)
    if not 0 < encoded < 1 << 63:
        raise ValueError("sample rate is not representable in VRT Q20")
    return struct.pack(">IIIIIq", (6 << 28) | 7, stream_id, (2 << 23) | (1 << 19), message_id, 1 << 21, encoded)


def parse_ack(data, stream_id, message_id):
    if len(data) != 60:
        raise ValueError("unexpected ACK length")
    header, stream = struct.unpack_from(">II", data)
    cam, msg, cif = struct.unpack_from(">III", data, 20)
    if (header >> 28 != 6 or not header & (1 << 26) or (header & 0xffff) * 4 != len(data)
            or stream != stream_id or msg != message_id or cif != 0x28a00000):
        raise ValueError("unexpected ACK header, stream, message ID or fields")
    if cam & (1 << 16) or not cam & (1 << 10):
        raise ValueError("collector rejected rate command")
    actual = struct.unpack_from(">q", data, 52)[0] / SCALE
    if actual <= 0:
        raise ValueError("invalid sample rate readback")
    return actual, bool(cam & (1 << 11))


def receive_ack(sock, stream_id, message_id, timeout):
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("timeout must be finite and positive")
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("no matching acknowledgement before deadline")
        sock.settimeout(remaining)
        data = sock.recv(65535)
        # A timed-out previous controller can leave a late ACK for this fixed
        # source port. Ignore unrelated responses, never resend the command.
        if len(data) < 28:
            continue
        stream = struct.unpack_from(">I", data, 4)[0]
        message = struct.unpack_from(">I", data, 24)[0]
        if stream != stream_id or message != message_id:
            continue
        return parse_ack(data, stream_id, message_id)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("rate", type=float, help="requested complex samples per second")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=4991)
    parser.add_argument("--bind", default="127.0.0.1", help="must match authorized_controller IP")
    parser.add_argument("--source-port", type=int, default=50000, help="must match authorized_controller port")
    parser.add_argument("--stream-id", type=lambda s: int(s, 0), default=1)
    parser.add_argument("--timeout", type=float, default=3)
    args = parser.parse_args()
    try:
        if not math.isfinite(args.timeout) or args.timeout <= 0:
            raise ValueError("timeout must be finite and positive")
        msg = secrets.randbits(32)
        packet = request(args.stream_id, args.rate, msg)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
            sock.bind((args.bind, args.source_port))
            sock.connect((args.host, args.port))
            sock.send(packet)
            actual, partial = receive_ack(sock, args.stream_id, msg, args.timeout)
        print(f"requested={args.rate:g} samples/s actual={actual:g} samples/s" + (" (hardware rounded or partially applied)" if partial else ""))
    except (OSError, ValueError, OverflowError, struct.error) as exc:
        print(f"rate control failed: {exc}; timeout does not prove the command was not applied", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
