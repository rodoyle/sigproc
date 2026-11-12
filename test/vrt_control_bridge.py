#!/usr/bin/env python3
"""Bounded loopback UDP <-> framed SSH stdio bridge for the VRT control profile.

Topology:

    Soapy ephemeral-UDP source -> Windows 127.0.0.1:<ctrl_port>  (local mode)
      -> 4-byte length-framed SSH stdio (forced command on a dedicated account)
      -> node 127.0.0.1:50000 source port (remote mode)
      -> collector 127.0.0.1:4991 fixed authorized_controller ACL.

A plain `ssh -L` TCP forward cannot preserve the collector's fixed source-port ACL,
pin one controller peer, or reject unsupported commands, so the bridge relays VRT
datagrams in explicit frames and enforces the Soapy/Rust command profile on both
ends. Both ends are query-only unless each is started with --allow-execute.

Nothing here installs, deploys, or opens a device: it only moves already-validated
frames between loopback sockets and an operator-supplied SSH subprocess.
"""
import argparse
import contextlib
import dataclasses
import ipaddress
import math
import os
import queue
import re
import secrets
import socket
import struct
import subprocess
import sys
import threading
import time

TYPE_COMMAND = 6
TYPE_SHIFT = 28
ACK_FLAG = 1 << 26
# A command must not carry acknowledge/cancel/reserved header bits (see
# common/src/vita49_control.rs header_ok).
COMMAND_FORBIDDEN_HEADER_BITS = (1 << 26) | (1 << 25) | (1 << 24)
CAM_QUERY_STATE = 1 << 18
CAM_EXECUTION_ACK = 1 << 19
CAM_EXECUTED = 1 << 10
CAM_PARTIAL_ACTION = 1 << 11
CAM_ERRORS = 1 << 16
BW = 1 << 29
RF = 1 << 27
GAIN = 1 << 23
RATE = 1 << 21
SUPPORTED = BW | RF | GAIN | RATE
RESERVED_CAM_BITS = (1 << 30) | (1 << 28) | (7 << 12)
ACTION_EXECUTE = 2
SCALE = 1 << 20
MAX_FRAME = 4096
MAX_DATAGRAM = 65535
MAX_TIMEOUT = 60.0
MAX_IGNORED = 1024
SSH_METACHARACTERS = frozenset(" \t\r\n'\";|&$`<>(){}[]*?!\\")

class DiagnosticSink:
    """Best-effort bounded diagnostics; stderr can never stall control handling."""

    def __init__(self, stream):
        self._messages = queue.Queue(maxsize=8)
        self._drained = threading.Event()
        self._drained.set()
        try:
            self._fd = stream.fileno()
        except (AttributeError, OSError, ValueError):
            self._fd = None
        if self._fd is not None:
            threading.Thread(target=self._pump, name="vrt-bridge-log", daemon=True).start()

    def __call__(self, message):
        if self._fd is None:
            return
        record = str(message).encode("utf-8", errors="replace")[:4095] + b"\n"
        self._drained.clear()
        try:
            self._messages.put_nowait(record)
        except queue.Full:
            return  # Dropping diagnostics is safer than blocking the control socket.

    def _pump(self):
        fd = self._fd
        if fd is None:
            return
        while True:
            record = memoryview(self._messages.get())
            try:
                while record:
                    count = os.write(fd, record)
                    if count <= 0:
                        raise OSError("diagnostic pipe made no progress")
                    record = record[count:]
            except OSError:
                self._drained.set()
                return
            if self._messages.empty():
                self._drained.set()

    def flush(self, timeout=0.05):
        self._drained.wait(timeout)


diagnostic = DiagnosticSink(sys.stderr)



class BridgeError(Exception):
    """Base class for bridge failures that must fail closed."""


class FrameProtocolError(BridgeError):
    """The 4-byte length framing itself is invalid."""


class FrameTruncated(FrameProtocolError):
    """The stream ended inside a frame."""


class StreamClosed(BridgeError):
    """The transport ended cleanly at a frame boundary."""


class TransportError(BridgeError):
    """The transport pipe failed."""


class TransportTimeout(TransportError):
    """No transport bytes arrived before the exchange deadline."""


class MalformedCommand(BridgeError):
    """Datagram is not a well-formed VRT command; drop it without a reply."""


class CommandRejected(BridgeError):
    """Well-formed command outside the bounded profile; answer with an error ACK."""


class AckTimeout(BridgeError):
    """No correlated acknowledgement before the deadline; never retried."""


class ProtocolError(BridgeError):
    """A relayed frame is not the acknowledgement for the outstanding request."""


@dataclasses.dataclass(frozen=True)
class Limits:
    timeout: float = 3.0
    stream_id: int = 1
    allow_execute: bool = False
    max_ignored: int = 32

    def validate(self):
        if not math.isfinite(self.timeout) or not 0 < self.timeout <= MAX_TIMEOUT:
            raise ValueError(f"timeout must be finite in (0, {MAX_TIMEOUT:g}]")
        if not 0 < self.stream_id < 1 << 32:
            raise ValueError("stream ID must be a nonzero 32-bit value")
        if not 0 <= self.max_ignored <= MAX_IGNORED:
            raise ValueError(f"max_ignored must be within [0, {MAX_IGNORED}]")


@dataclasses.dataclass(frozen=True)
class Command:
    header: int
    stream_id: int
    cam: int
    message_id: int
    message_id_offset: int
    id_flags: int
    ids: bytes
    controllee_id: int | None
    cif0: int
    body: bytes
    values: dict


def _word(buf, offset):
    return struct.unpack_from(">I", buf, offset)[0]


def parse_command(datagram):
    """Parse the bounded Soapy/Rust type 6 command profile.

    Mirrors common/src/vita49_control.rs parse_command: optional class ID and
    timestamps move the CAM/Message ID offset, so it is never assumed.
    """
    if len(datagram) < 8:
        raise MalformedCommand("command is shorter than the VRT prologue")
    header = _word(datagram, 0)
    if header >> TYPE_SHIFT != TYPE_COMMAND:
        raise MalformedCommand(f"packet type {header >> TYPE_SHIFT} is not a command")
    if (header & 0xFFFF) * 4 != len(datagram):
        raise MalformedCommand("declared word count disagrees with the datagram length")
    offset = 8 + (8 if header & (1 << 27) else 0)
    offset += 4 if (header >> 22) & 3 else 0
    offset += 8 if (header >> 20) & 3 else 0
    if offset + 8 > len(datagram):
        raise MalformedCommand("command ends before CAM and Message ID")
    cam = _word(datagram, offset)
    message_id_offset = offset + 4
    message_id = _word(datagram, offset + 4)
    offset += 8
    ids_start = offset
    controllee_id = None
    for present, uuid in ((1 << 31, 1 << 30), (1 << 29, 1 << 28)):
        if cam & present == 0:
            continue
        size = 16 if cam & uuid else 4
        if offset + size > len(datagram):
            raise MalformedCommand("command ends inside its ID list")
        if present == 1 << 31 and size == 4:
            controllee_id = _word(datagram, offset)
        offset += size
    ids = bytes(datagram[ids_start:offset])
    if offset + 4 > len(datagram):
        raise MalformedCommand("command ends before CIF0")
    cif0 = _word(datagram, offset)
    offset += 4
    body_start = offset
    values = {}
    if cif0 & ~SUPPORTED == 0:
        # Unknown fields have unknown lengths: reject the command before reading
        # any field body, exactly like the controllee.
        for name, bit in (("bandwidth_hz", BW), ("rf_freq_hz", RF),
                          ("gain_db", GAIN), ("sample_rate_hz", RATE)):
            if cif0 & bit == 0:
                continue
            size = 4 if bit == GAIN else 8
            if offset + size > len(datagram):
                raise MalformedCommand("command ends inside a field body")
            if bit == GAIN:
                values[name] = struct.unpack_from(">h", datagram, offset + 2)[0] / 128.0
            else:
                values[name] = struct.unpack_from(">q", datagram, offset)[0] / SCALE
            offset += size
        if offset != len(datagram):
            raise MalformedCommand("command has trailing words")
    return Command(header=header, stream_id=_word(datagram, 4), cam=cam,
                   message_id=message_id, message_id_offset=message_id_offset, id_flags=cam & 0xF0000000, ids=ids,
                   controllee_id=controllee_id, cif0=cif0,
                   body=bytes(datagram[body_start:offset]), values=values)


def check_command(command, limits):
    """Enforce the profile shared by both ends; returns the action mode.

    Raises CommandRejected for well-framed commands that get a synthesized error
    ACK, so the plugin can preserve state instead of waiting for a timeout.
    """
    if command.header & COMMAND_FORBIDDEN_HEADER_BITS:
        raise CommandRejected("acknowledge/cancel/reserved header bits are not valid in a command")
    if command.cam & RESERVED_CAM_BITS:
        raise CommandRejected("reserved CAM bits are set")
    if command.stream_id != limits.stream_id:
        raise CommandRejected(f"stream ID {command.stream_id} is not the configured {limits.stream_id}")
    if command.controllee_id is not None and command.controllee_id != limits.stream_id:
        raise CommandRejected("controllee ID does not match the configured stream")
    if command.cif0 == 0 or command.cif0 & ~SUPPORTED:
        raise CommandRejected("command sets fields outside the four supported CIF0 settings")
    action = (command.cam >> 23) & 3
    query = bool(command.cam & CAM_QUERY_STATE)
    if action == 0 and query:
        return action
    if action != ACTION_EXECUTE or query:
        raise CommandRejected("contradictory or unsupported query/execute mode")
    if not limits.allow_execute:
        raise CommandRejected("execute is not enabled on this end (query-only)")
    return action


def build_error_ack(command):
    """Synthesize the controllee's 'execution ACK with errors' for a rejection.

    Identity (stream, message ID and ID flags) is echoed so the plugin can
    correlate. CIF0 is empty: the relay has no authoritative hardware readback
    and must not echo a rejected request as if it were receiver state. No
    successful ACK is ever synthesized.
    """
    cam = CAM_EXECUTION_ACK | CAM_ERRORS | command.id_flags
    body = command.ids + struct.pack(">I", 0)
    header = ((TYPE_COMMAND << TYPE_SHIFT) | ACK_FLAG | (3 << 22) | (1 << 20)
              | (7 + len(body) // 4))
    ack = struct.pack(">5I", header, command.stream_id, 0, 0, 0)
    ack += struct.pack(">II", cam, command.message_id) + body
    if len(ack) > MAX_FRAME:
        raise FrameProtocolError("synthesized error ACK exceeds the frame cap")
    return ack


def ack_identity(datagram):
    """Return (stream_id, message_id, cam) for a type 6 ACK, else None."""
    if len(datagram) < 16:
        return None
    header = _word(datagram, 0)
    if header >> TYPE_SHIFT != TYPE_COMMAND or not header & ACK_FLAG:
        return None
    if (header & 0xFFFF) * 4 != len(datagram):
        return None
    offset = 8 + (8 if header & (1 << 27) else 0)
    offset += 4 if (header >> 22) & 3 else 0
    offset += 8 if (header >> 20) & 3 else 0
    if offset + 8 > len(datagram):
        return None
    return _word(datagram, 4), _word(datagram, offset + 4), _word(datagram, offset)


def check_relayed_ack(datagram, command):
    """Validate the full ACK, its echoed IDs, state fields and outcome."""
    if ack_identity(datagram) is None:
        raise ProtocolError("relayed frame is not a VRT acknowledgement")
    try:
        reply = parse_command(datagram)
    except MalformedCommand as exc:
        raise ProtocolError(f"incomplete acknowledgement: {exc}") from exc
    if reply.header & ((1 << 25) | (1 << 24)):
        raise ProtocolError("acknowledgement carries unsupported header flags")
    if reply.cif0 & ~SUPPORTED:
        raise ProtocolError("acknowledgement carries unsupported CIF0 fields")
    if (reply.stream_id, reply.message_id) != (command.stream_id, command.message_id):
        raise ProtocolError("acknowledgement does not match the outstanding request")
    if reply.id_flags != command.id_flags or reply.ids != command.ids:
        raise ProtocolError("acknowledgement does not echo the request identifiers")
    cam = reply.cam
    if cam & CAM_ERRORS:
        # Rust and bridge policy denials use execution-error ACKs, including
        # denials of queries. They cannot claim an applied operation.
        if not cam & CAM_EXECUTION_ACK or cam & (CAM_EXECUTED | CAM_QUERY_STATE | CAM_PARTIAL_ACTION):
            raise ProtocolError("invalid error acknowledgement outcome")
        return cam
    if (reply.cif0 & command.cif0) != command.cif0:
        raise ProtocolError("successful acknowledgement omits requested state fields")
    for name, value in reply.values.items():
        if name != "gain_db" and value <= 0:
            raise ProtocolError("successful acknowledgement has invalid state readback")
    if ((command.cam >> 23) & 3) == 0 and command.cam & CAM_QUERY_STATE:
        if not cam & CAM_QUERY_STATE or cam & (CAM_EXECUTION_ACK | CAM_EXECUTED | CAM_PARTIAL_ACTION):
            raise ProtocolError("query acknowledgement claims the wrong outcome")
    elif not cam & CAM_EXECUTION_ACK or not cam & CAM_EXECUTED or cam & CAM_QUERY_STATE:
        raise ProtocolError("execute acknowledgement claims the wrong outcome")
    if ((command.cam >> 23) & 3) == ACTION_EXECUTE:
        changed = any(reply.values.get(name) != value for name, value in command.values.items())
        if changed and not cam & CAM_PARTIAL_ACTION:
            raise ProtocolError("mismatched execute readback requires partial-action bit11")
    return cam


def rewrite_message_id(datagram, command, message_id):
    packet = bytearray(datagram)
    struct.pack_into(">I", packet, command.message_id_offset, message_id)
    return bytes(packet)

def pack_frame(payload):
    if not isinstance(payload, (bytes, bytearray)):
        raise FrameProtocolError("frame payload must be bytes")
    if not 0 < len(payload) <= MAX_FRAME:
        raise FrameProtocolError(f"frame length {len(payload)} is outside 1..{MAX_FRAME}")
    return len(payload).to_bytes(4, "big") + bytes(payload)


def _bounded_io(operation, timeout):
    """One attempt only; daemon I/O cannot hold up fail-closed process teardown."""
    result = queue.Queue(maxsize=1)

    def run():
        try:
            operation()
        except BaseException as exc:
            result.put(exc)
        else:
            result.put(None)

    threading.Thread(target=run, name="vrt-bridge-write", daemon=True).start()
    try:
        outcome = result.get(timeout=timeout)
    except queue.Empty:
        raise TransportTimeout("transport write/close deadline expired; no retry") from None
    if isinstance(outcome, BaseException):
        raise TransportError(f"transport I/O failed: {outcome}") from outcome


def _write_all(stream, payload):
    try:
        fd = stream.fileno()
    except (AttributeError, OSError, ValueError):
        fd = None
    remaining = memoryview(payload)
    while remaining:
        count = os.write(fd, remaining) if fd is not None else stream.write(remaining)
        if count is None or count <= 0:
            raise TransportError("transport made no write progress")
        remaining = remaining[count:]
    # Production pipes use raw descriptor writes. No buffered lock remains held
    # when a timed-out writer is abandoned during CLI process shutdown.
    if fd is None:
        stream.flush()


def write_frame(stream, payload, timeout=3.0):
    framed = pack_frame(payload)
    _bounded_io(lambda: _write_all(stream, framed), timeout)

class ChunkReader:
    """Pump a pipe on a daemon thread so Windows pipes have real deadlines.

    selectors cannot wait on Windows pipe handles, so the blocking read lives in a
    thread and the exchange thread only waits on a queue with a timeout.
    """

    def __init__(self, stream):
        self._stream = stream
        self._chunks = queue.Queue(maxsize=2)
        self._buffer = b""
        self._closed = False
        self._stopped = threading.Event()
        threading.Thread(target=self._pump, name="vrt-bridge-pipe", daemon=True).start()

    def _read_some(self):
        try:
            fd = self._stream.fileno()
        except (AttributeError, OSError, ValueError):
            fd = None
        if fd is not None:
            return os.read(fd, 4096)
        read1 = getattr(self._stream, "read1", None)
        if read1 is not None:
            return read1(4096)
        return self._stream.read(1)

    def close(self):
        self._stopped.set()

    def _emit(self, item):
        while not self._stopped.is_set():
            try:
                self._chunks.put(item, timeout=0.1)
                return True
            except queue.Full:
                continue
        return False

    def _pump(self):
        while not self._stopped.is_set():
            try:
                chunk = self._read_some()
            except (OSError, ValueError) as exc:
                self._closed = True
                self._emit(exc)
                return
            if not chunk:
                self._closed = True
                self._emit(None)
                return
            if not self._emit(chunk):
                return

    def _next_chunk(self, deadline):
        if self._closed and self._chunks.empty():
            return None
        timeout = None
        if deadline is not None:
            timeout = deadline - time.monotonic()
            if timeout <= 0:
                raise TransportTimeout("no transport bytes before the exchange deadline")
        try:
            return self._chunks.get(timeout=timeout)
        except queue.Empty:
            raise TransportTimeout("no transport bytes before the exchange deadline") from None

    def read_exact(self, size, deadline=None):
        out = bytearray()
        while len(out) < size:
            if self._buffer:
                take = min(size - len(out), len(self._buffer))
                out += self._buffer[:take]
                self._buffer = self._buffer[take:]
                continue
            item = self._next_chunk(deadline)
            if item is None:
                if out:
                    raise FrameTruncated(f"transport ended after {len(out)} of {size} bytes")
                raise StreamClosed("transport reached an orderly EOF")
            if isinstance(item, BaseException):
                raise TransportError(f"transport read failed: {item}") from item
            self._buffer = item
        return bytes(out)


class FrameReader:
    def __init__(self, stream):
        self._chunks = ChunkReader(stream)

    def close(self):
        self._chunks.close()

    def read_frame(self, timeout: float | None = None, allow_idle: bool = False):
        """Idle may be unlimited; once a frame starts, all bytes are bounded."""
        if allow_idle:
            if timeout is None:
                raise ValueError("idle framing requires a bounded frame timeout")
            try:
                first = self._chunks.read_exact(1)
            except StreamClosed:
                return None
            deadline = time.monotonic() + timeout
            try:
                prefix = first + self._chunks.read_exact(3, deadline)
            except StreamClosed as exc:
                raise FrameTruncated("transport ended inside the frame prefix") from exc
        else:
            deadline = None if timeout is None else time.monotonic() + timeout
            try:
                prefix = self._chunks.read_exact(4, deadline)
            except StreamClosed:
                return None
        length = int.from_bytes(prefix, "big")
        if not 0 < length <= MAX_FRAME:
            raise FrameProtocolError(f"frame length {length} is outside 1..{MAX_FRAME}")
        try:
            return self._chunks.read_exact(length, deadline)
        except StreamClosed as exc:
            raise FrameTruncated("transport ended before the framed payload") from exc

class SshTransport:
    """Frame exchange over an SSH subprocess carrying a forced-command endpoint.

    The process is any handle with stdin/stdout and poll/terminate/kill/wait, so
    tests inject a local helper subprocess or an in-memory handle instead of
    weakening the SSH security options used by main().
    """

    def __init__(self, process):
        self._process = process
        self._stdin = process.stdin
        self.reader = FrameReader(process.stdout)

    def exchange(self, payload, timeout):
        deadline = time.monotonic() + timeout
        write_frame(self._stdin, payload, timeout)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TransportTimeout("exchange deadline expired after write; no retry")
        reply = self.reader.read_frame(remaining)
        if reply is None:
            raise StreamClosed("remote endpoint closed before answering the request")
        return reply

    def close(self, force=False):
        """Bound cleanup even if a pipe writer or a peer has stalled."""
        self.reader.close()
        process = self._process
        if not force:
            with contextlib.suppress(BridgeError):
                _bounded_io(self._stdin.close, 0.5)
            with contextlib.suppress(OSError, ValueError, subprocess.TimeoutExpired):
                process.wait(timeout=2.0)
        # On timeout terminate the peer before closing handles used by I/O
        # threads, so raw writes can unblock without a buffered-stream lock.
        with contextlib.suppress(OSError, ValueError, subprocess.TimeoutExpired):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=2.0)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=2.0)
        for stream in (self._stdin, process.stdout):
            with contextlib.suppress(BridgeError):
                _bounded_io(stream.close, 0.5)


class LocalBridge:
    """Client end: pinned loopback UDP peer in, framed SSH exchange out."""

    def __init__(self, sock, transport, limits, log=None):
        self._sock = sock
        self._transport = transport
        self._limits = limits
        self._log = log or diagnostic
        self.peer = None

    def _reply(self, datagram, peer):
        # Always answer from the bound listener socket so the plugin's ephemeral
        # source port is honoured and no second socket can cross-deliver.
        self._sock.sendto(datagram, peer)

    def serve(self, max_datagrams=None):
        handled = 0
        while max_datagrams is None or handled < max_datagrams:
            datagram, source = self._sock.recvfrom(MAX_DATAGRAM)
            if self.peer is not None and source != self.peer:
                self._log(f"dropped datagram from unpinned peer {source[0]}:{source[1]}")
                continue
            handled += 1
            try:
                command = parse_command(datagram)
            except MalformedCommand as exc:
                self._log(f"dropped malformed datagram: {exc}")
                continue
            try:
                check_command(command, self._limits)
            except CommandRejected as exc:
                self._log(f"rejected command without forwarding: {exc}")
                self._reply(build_error_ack(command), source)
                continue
            if self.peer is None:
                self.peer = source
                self._log(f"pinned validated control peer {source[0]}:{source[1]}")
            reply = self._transport.exchange(datagram, self._limits.timeout)
            check_relayed_ack(reply, command)
            self._reply(reply, source)
        return handled


class MessageIds:
    """A random session start plus a non-repeating 32-bit counter.

    Client IDs are never used for node-side correlation. IDs are not
    authentication or durable replay protection across arbitrary restarts.
    """

    def __init__(self):
        self._next = secrets.randbits(32)
        self._remaining = 1 << 32

    def take(self):
        if self._remaining == 0:
            raise ProtocolError("bridge message ID space exhausted; start a new session")
        current = self._next
        self._next = (current + 1) & 0xFFFFFFFF
        self._remaining -= 1
        return current


class RemoteBridge:
    """Node end: framed stdin requests out to the collector, ACK frames back."""

    def __init__(self, sock, reader, writer, limits, log=None):
        self._sock = sock
        self._reader = reader
        self._writer = writer
        self._limits = limits
        self._message_ids = MessageIds()
        self._log = log or diagnostic

    def await_ack(self, stream_id, message_id):
        deadline = time.monotonic() + self._limits.timeout
        ignored = 0
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AckTimeout(f"no matching acknowledgement within {self._limits.timeout:g}s")
            self._sock.settimeout(remaining)
            try:
                datagram = self._sock.recv(MAX_DATAGRAM)
            except TimeoutError:
                raise AckTimeout(
                    f"no matching acknowledgement within {self._limits.timeout:g}s; "
                    "a timed-out command may still have been applied and is never retried"
                ) from None
            identity = ack_identity(datagram)
            if identity is None or identity[0] != stream_id or identity[1] != message_id:
                ignored += 1
                self._log(f"ignored unrelated or stale datagram ({ignored})")
                if ignored > self._limits.max_ignored:
                    raise AckTimeout("too many unrelated datagrams before the matching acknowledgement")
                continue
            return datagram

    def handle(self, frame):
        """Answer one framed request with one framed acknowledgement."""
        try:
            command = parse_command(frame)
        except MalformedCommand as exc:
            raise ProtocolError(f"rejected framed request: {exc}") from None
        try:
            check_command(command, self._limits)
        except CommandRejected as exc:
            self._log(f"rejected command without forwarding: {exc}")
            return build_error_ack(command)
        wire_id = self._message_ids.take()
        if wire_id == command.message_id:
            wire_id = self._message_ids.take()
        wire_command = dataclasses.replace(command, message_id=wire_id)
        self._sock.send(rewrite_message_id(frame, command, wire_id))
        reply = self.await_ack(command.stream_id, wire_id)
        check_relayed_ack(reply, wire_command)
        return rewrite_message_id(reply, parse_command(reply), command.message_id)

    def serve(self):
        handled = 0
        while True:
            frame = self._reader.read_frame(self._limits.timeout, allow_idle=True)
            if frame is None:
                self._log("stdin closed at a frame boundary; exiting")
                return handled
            deadline = time.monotonic() + self._limits.timeout
            reply = self.handle(frame)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TransportTimeout("remote exchange deadline expired before reply")
            write_frame(self._writer, reply, remaining)
            handled += 1


def loopback_endpoint(host, port, name, allow_ephemeral=False):
    """Validate a numeric IPv4 loopback endpoint before any socket is bound."""
    try:
        address = ipaddress.IPv4Address(host)
    except ipaddress.AddressValueError:
        raise ValueError(f"{name} must be a numeric IPv4 address, not {host!r}") from None
    if not address.is_loopback:
        raise ValueError(f"{name} must be a loopback address, not {address}")
    if not 0 <= port <= 65535 or (port == 0 and not allow_ephemeral):
        raise ValueError(f"{name} port must be a valid UDP port, not {port}")
    return (str(address), port)


def ssh_argv(target, identity, connect_timeout):
    """Build the SSH argv list; no remote command string is ever appended.

    A dedicated account's authorized_keys forced command starts the remote helper,
    so OpenSSH never routes caller text through a remote shell. This function only
    validates the operator-supplied target/identity and returns an argv list that
    is executed with shell=False.
    """
    if not isinstance(target, str) or not 0 < len(target) <= 255:
        raise ValueError("ssh target must be a nonempty configured host alias")
    if target.startswith("-") or SSH_METACHARACTERS & set(target):
        raise ValueError("ssh target must not start with '-' or contain shell characters")
    if not re.fullmatch(r"[A-Za-z0-9_.:@%+-]+", target):
        raise ValueError("ssh target may contain only host/user/port characters")
    if not isinstance(identity, str) or not 0 < len(identity) <= 4096:
        raise ValueError("identity file must be an explicitly supplied path")
    if identity.startswith("-") or any(ch in identity for ch in "\x00\r\n"):
        raise ValueError("identity file path is not a safe argv value")
    if not math.isfinite(connect_timeout) or not 0 < connect_timeout <= MAX_TIMEOUT:
        raise ValueError(f"connect timeout must be finite in (0, {MAX_TIMEOUT:g}]")
    return ["ssh", "-T",
            "-o", "BatchMode=yes",
            "-o", "StrictHostKeyChecking=yes",
            "-o", "IdentitiesOnly=yes",
            "-o", "IdentityAgent=none",
            "-o", f"ConnectTimeout={max(1, math.ceil(connect_timeout))}",
            "-o", "ClearAllForwardings=yes",
            "-o", "ForwardAgent=no",
            "-o", "ForwardX11=no",
            "-o", "PermitLocalCommand=no",
            "-i", identity,
            target]


def _limits_from_args(args):
    limits = Limits(timeout=args.timeout, stream_id=args.stream_id,
                    allow_execute=args.allow_execute, max_ignored=args.max_ignored)
    limits.validate()
    return limits


def run_local_mode(args):
    limits = _limits_from_args(args)
    if limits.allow_execute and not args.trusted_local_session:
        raise ValueError("execute requires --trusted-local-session and OS-level client isolation")
    endpoint = loopback_endpoint(args.bind, args.local_port, "local listener",
                                allow_ephemeral=True)
    argv = ssh_argv(args.ssh_target, args.identity, args.connect_timeout)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(endpoint)
        diagnostic(f"local control bridge listening on {sock.getsockname()[0]}:{sock.getsockname()[1]}")
        diagnostic("ssh argv: " + " ".join(argv))
        process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   bufsize=0, shell=False)
        transport = SshTransport(process)
        try:
            LocalBridge(sock, transport, limits).serve(args.max_datagrams)
        except BaseException:
            transport.close(force=True)
            raise
        transport.close()
    return 0


def run_remote_mode(args):
    limits = _limits_from_args(args)
    endpoint = loopback_endpoint(args.bind, args.source_port, "collector source",
                                allow_ephemeral=True)
    collector = loopback_endpoint(args.collector, args.collector_port, "collector")
    reader = FrameReader(sys.stdin.buffer)
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
        sock.bind(endpoint)
        sock.connect(collector)
        diagnostic(f"remote control bridge source {sock.getsockname()[0]}:{sock.getsockname()[1]}"
                   f" -> collector {collector[0]}:{collector[1]}")
        RemoteBridge(sock, reader, sys.stdout.buffer, limits).serve()
    return 0


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    modes = parser.add_subparsers(dest="mode", required=True)

    local = modes.add_parser("local", help="bind the Windows loopback control port")
    local.add_argument("--bind", default="127.0.0.1", help="numeric loopback listener address")
    local.add_argument("--local-port", type=int, required=True,
                       help="control port the Soapy plugin sends to (0 = ephemeral, tests only)")
    local.add_argument("--ssh-target", required=True,
                       help="dedicated SSH host alias whose forced command runs the remote helper")
    local.add_argument("--identity", required=True, help="dedicated private key file")
    local.add_argument("--connect-timeout", type=float, default=5.0)
    local.add_argument("--timeout", type=float, default=3.0, help="bounded exchange timeout")
    local.add_argument("--stream-id", type=lambda text: int(text, 0), default=1)
    local.add_argument("--allow-execute", action="store_true",
                       help="permit mutations; must also be enabled on the node")
    local.add_argument("--trusted-local-session", action="store_true",
                       help="assert an OS-isolated trusted desktop session; UDP pinning is not authentication")
    local.add_argument("--max-ignored", type=int, default=32)
    local.add_argument("--max-datagrams", type=int, default=None,
                       help="stop after N handled datagrams (tests only)")
    local.set_defaults(handler=run_local_mode)

    remote = modes.add_parser("remote", help="forced-command node endpoint")
    remote.add_argument("--bind", default="127.0.0.1", help="numeric loopback source address")
    remote.add_argument("--source-port", type=int, default=50000,
                        help="must equal the collector's authorized_controller port")
    remote.add_argument("--collector", default="127.0.0.1")
    remote.add_argument("--collector-port", type=int, default=4991)
    remote.add_argument("--timeout", type=float, default=3.0, help="bounded exchange timeout")
    remote.add_argument("--stream-id", type=lambda text: int(text, 0), default=1)
    remote.add_argument("--allow-execute", action="store_true",
                        help="permit mutations; fixed in the account's forced-command config")
    remote.add_argument("--max-ignored", type=int, default=32)
    remote.set_defaults(handler=run_remote_mode)
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        return args.handler(args)
    except (BridgeError, OSError, ValueError, subprocess.SubprocessError) as exc:
        diagnostic(f"vrt control bridge failed: {exc}")
        diagnostic.flush()
        return 1


if __name__ == "__main__":
    sys.exit(main())
