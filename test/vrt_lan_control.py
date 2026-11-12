#!/usr/bin/env python3
"""Native Windows trusted-LAN VRT control client for the sigproc collector.

Topology (direct UDP packets, no SSH and no WSL):

    Soapy plugin -> 127.0.0.1:5599          optional loopback relay, query-only by default
      -> 192.168.0.15:50000                 bound controller source (Ethernet4, index 17)
      -> 192.168.0.55:4991                  collector [control] ctrl_addr/ctrl_port
      -> authorized_controller              = "192.168.0.15:50000", stream_id = 1

Protocol logic is not reimplemented here. Commands, the bounded profile, complete
ACK/readback validation, partial-action bit 11 and error outcomes come from
`vrt_control_bridge.parse_command`, `check_command`, `check_relayed_ack` and
`RemoteBridge.handle` (fresh wire message IDs, restored client IDs, no retries).
This module adds only the native LAN transport, explicit subnet/peer validation,
the read/tune/hold/restore workflow and the optional query-only loopback relay.

The collector's source-IP/port allowlist is not authentication or replay
protection, and 127.0.0.1 loopback peering is not authentication either. Use this
only on the operator-approved trusted 192.168.0.0/24 LAN with an OS-isolated
Windows host. Nothing here opens a radio, deploys a workload, or changes a
firewall; execute defaults off and must be requested explicitly on every end.
"""
import argparse
import ctypes
import dataclasses
import ipaddress
import math
import os
import socket
import struct
import sys
import threading
import time

import vrt_control_bridge as bridge

report = bridge.diagnostic

AUTHORIZED_SUBNET = ipaddress.IPv4Network("192.168.0.0/24")
CONTROLLER_ADDRESS = "192.168.0.15"
CONTROLLER_PORT = 50000
COLLECTOR_ADDRESS = "192.168.0.55"
COLLECTOR_PORT = 4991
DEFAULT_RELAY_ADDR = "127.0.0.1"
DEFAULT_RELAY_PORT = 5599
DEFAULT_STREAM_ID = 1
MAX_HOLD_SECONDS = 120.0
DEMO_FREQUENCY_MHZ = 145.310
ALTERNATE_FREQUENCY_MHZ = 448.0
BROADCAST = ipaddress.IPv4Address("255.255.255.255")


@dataclasses.dataclass(frozen=True)
class LanProfile:
    """One exact controller/collector peer pair inside one authorized subnet."""

    subnet: ipaddress.IPv4Network = AUTHORIZED_SUBNET
    controller: tuple[str, int] = (CONTROLLER_ADDRESS, CONTROLLER_PORT)
    collector: tuple[str, int] = (COLLECTOR_ADDRESS, COLLECTOR_PORT)

    def endpoints(self):
        return (lan_endpoint(*self.controller, name="controller", subnet=self.subnet),
                lan_endpoint(*self.collector, name="collector", subnet=self.subnet))


LAN_PROFILE = LanProfile()

_PROCESS_SESSION_LOCK = threading.Lock()


class SessionBusy(bridge.BridgeError):
    """Another cooperative controller owns the complete control session."""


class SessionLock:
    """Windows kernel mutex held across query, tune, hold and fresh-socket restore.

    Shared with the relay and standalone queries, including across processes.
    This coordinates clients; it is not authentication of hostile local senders.
    """

    def __init__(self, profile=LAN_PROFILE):
        self.name = f"Global\\SigprocVRT-{profile.controller[0]}-{profile.controller[1]}"
        self._api = None
        self._handle = None

    def __enter__(self):
        if not _PROCESS_SESSION_LOCK.acquire(blocking=False):
            raise SessionBusy("another query/tune/relay session is active")
        try:
            if os.name != "nt":
                raise bridge.BridgeError("this controller uses a native Windows session mutex")
            api = ctypes.WinDLL("kernel32", use_last_error=True)
            api.CreateMutexW.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_wchar_p]
            api.CreateMutexW.restype = ctypes.c_void_p
            api.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
            api.WaitForSingleObject.restype = ctypes.c_ulong
            api.ReleaseMutex.argtypes = [ctypes.c_void_p]
            api.CloseHandle.argtypes = [ctypes.c_void_p]
            handle = api.CreateMutexW(None, False, self.name)
            if not handle:
                raise ctypes.WinError(ctypes.get_last_error())
            result = api.WaitForSingleObject(handle, 0)
            if result not in (0, 0x80):
                api.CloseHandle(handle)
                raise SessionBusy("another Windows controller owns the source endpoint")
            if result == 0x80:
                report("previous controller exited unexpectedly; current RF must be queried")
            self._api, self._handle = api, handle
            return self
        except BaseException:
            _PROCESS_SESSION_LOCK.release()
            raise

    def __exit__(self, exc_type, exc_value, traceback):
        api, handle = self._api, self._handle
        try:
            if api is not None and handle is not None:
                try:
                    if not api.ReleaseMutex(handle):
                        raise ctypes.WinError(ctypes.get_last_error())
                finally:
                    api.CloseHandle(handle)
        finally:
            self._handle = None
            _PROCESS_SESSION_LOCK.release()
        return False



def lan_endpoint(host, port, name, subnet):
    """Validate one numeric unicast IPv4 peer inside the authorized subnet.

    Hostnames, unspecified, multicast and broadcast addresses are rejected, so a
    socket bind can never silently fall back to an unspecified interface.
    """
    if not isinstance(subnet, ipaddress.IPv4Network):
        raise ValueError("authorized subnet must be an IPv4 network")
    try:
        address = ipaddress.IPv4Address(host)
    except (ipaddress.AddressValueError, ValueError):
        raise ValueError(f"{name} must be a numeric IPv4 address, not {host!r}") from None
    if address.is_unspecified or address.is_multicast or address in (BROADCAST, subnet.network_address, subnet.broadcast_address):
        raise ValueError(f"{name} must be a unicast address, not {address}")
    if address not in subnet:
        raise ValueError(f"{name} {address} is outside the authorized subnet {subnet}")
    if not 0 < port <= 65535:
        raise ValueError(f"{name} port must be a fixed nonzero UDP port, not {port}")
    return (str(address), port)


def q20(hz):
    value = round(hz * bridge.SCALE)
    if not -(1 << 63) <= value < (1 << 63):
        raise ValueError("frequency is outside the signed Q20 wire range")
    return value


def build_command(stream_id, message_id, cam, cif0, fields=b""):
    """Build the bounded type 6 command profile (no class ID or timestamps)."""
    total = 20 + 8 + 4 + len(fields)
    if total % 4:
        raise ValueError("command field bodies must be whole 32-bit words")
    header = (bridge.TYPE_COMMAND << bridge.TYPE_SHIFT) | (3 << 22) | (1 << 20) | (total // 4)
    return (struct.pack(">5I", header, stream_id, 0, 0, 0)
            + struct.pack(">II", cam, message_id) + struct.pack(">I", cif0) + bytes(fields))


def query_command(stream_id, message_id):
    """Query-state for every supported setting; zeroed bodies are not requests."""
    return build_command(stream_id, message_id, bridge.CAM_QUERY_STATE, bridge.SUPPORTED,
                         b"\x00" * 28)


def tune_command(stream_id, message_id, rf_hz):
    """Immediate execute that changes only the RF frequency."""
    return build_command(stream_id, message_id, bridge.ACTION_EXECUTE << 23, bridge.RF,
                         struct.pack(">q", q20(rf_hz)))


def parse_and_check(datagram, limits):
    """Apply this end's profile before any datagram is sent."""
    command = bridge.parse_command(datagram)
    bridge.check_command(command, limits)
    return command


def open_control_socket(profile=LAN_PROFILE):
    """Bind the fixed controller source and connect to the fixed collector peer."""
    controller, collector = profile.endpoints()
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.bind(controller)
        sock.connect(collector)
    except BaseException:
        sock.close()
        raise
    return sock, controller, collector


class LanTransport:
    """Connected UDP exchange reusing `RemoteBridge`'s ID and ACK verification.

    `RemoteBridge.handle` replaces the client message ID with a fresh wire ID,
    waits for that exact ACK, validates the full reply/readback, and restores the
    client ID. The bridge's configured limit bounds the exchange; the timeout
    argument only keeps the `LocalBridge` transport interface.
    """

    def __init__(self, sock, limits, log=None):
        self._sock = sock
        self._remote = bridge.RemoteBridge(sock, None, None, limits, log=log)

    def exchange(self, payload, timeout):
        command = bridge.parse_command(payload)
        if ((command.cam >> 23) & 3) == bridge.ACTION_EXECUTE and command.cif0 != bridge.RF:
            return bridge.build_error_ack(command)
        return self._remote.handle(payload)

    def close(self):
        self._sock.close()


def exchange(sock, limits, datagram):
    """One bounded request/ACK exchange; a timeout is never retried."""
    command = parse_and_check(datagram, limits)
    reply = LanTransport(sock, limits).exchange(datagram, limits.timeout)
    cam = bridge.check_relayed_ack(reply, command)
    return cam, bridge.parse_command(reply).values


FIELDS = (("rf_freq_hz", "rf"), ("sample_rate_hz", "rate"),
          ("bandwidth_hz", "bandwidth"), ("gain_db", "gain"))


def describe(values):
    parts = []
    for key, label in FIELDS:
        if key not in values:
            continue
        if key == "gain_db":
            parts.append(f"{label}={values[key]:g}dB")
        else:
            parts.append(f"{label}={values[key] / 1e6:.12g}MHz ({values[key]:.15g}Hz)")
    return " ".join(parts) or "no state fields"


def require_trusted_lan(args, profile=LAN_PROFILE):
    if not args.trusted_lan:
        raise ValueError("direct LAN control requires --trusted-lan to acknowledge the "
                         f"authorized {profile.subnet} trusted network")


def limits_from_args(args, allow_execute):
    limits = bridge.Limits(timeout=args.timeout, stream_id=args.stream_id,
                           allow_execute=allow_execute, max_ignored=args.max_ignored)
    limits.validate()
    return limits


def _query_state_unlocked(args, profile=LAN_PROFILE):
    """Query-state only; no execute path exists in this mode."""
    require_trusted_lan(args, profile)
    limits = limits_from_args(args, allow_execute=False)
    sock, controller, collector = open_control_socket(profile)
    try:
        report(f"query {controller[0]}:{controller[1]} -> {collector[0]}:{collector[1]}")
        cam, values = exchange(sock, limits,
                               query_command(limits.stream_id, bridge.MessageIds().take()))
    finally:
        sock.close()
    if cam & bridge.CAM_ERRORS:
        raise bridge.CommandRejected("collector rejected the state query")
    report(f"query ACK cam=0x{cam:08X} {describe(values)}")
    return values

def query_state(args, profile=LAN_PROFILE):
    """Return validated state independently of lossy diagnostics."""
    with SessionLock(profile):
        return _query_state_unlocked(args, profile)


def query_frequency(args, profile=LAN_PROFILE):
    return query_state(args, profile)["rf_freq_hz"]


def run_query(args, profile=LAN_PROFILE):
    query_state(args, profile)
    return 0


def restore_frequency(profile, original_hz, limits):
    """Roll back with a fresh socket and fresh session IDs, never a target retry."""
    sock, _, _ = open_control_socket(profile)
    try:
        cam, values = exchange(sock, limits,
                               tune_command(limits.stream_id, bridge.MessageIds().take(),
                                            original_hz))
    finally:
        sock.close()
    if cam & bridge.CAM_ERRORS:
        raise bridge.CommandRejected("collector rejected the restore execute")
    readback = values.get("rf_freq_hz")
    if readback is None or q20(readback) != q20(original_hz):
        raise bridge.ProtocolError(f"restore readback {readback!r} Hz is not the original "
                                   f"{original_hz:g} Hz")
    return cam


def _run_tune_session(args, profile=LAN_PROFILE):
    """Tune RF only, show the acknowledged readback, hold finitely, restore origin."""
    require_trusted_lan(args, profile)
    if not args.allow_execute:
        raise ValueError("tune requires --allow-execute; this client is query-only by default")
    rf_hz = args.frequency_mhz * 1e6
    if not math.isfinite(rf_hz) or rf_hz <= 0:
        raise ValueError("--frequency-mhz must be finite and positive")
    q20(rf_hz)  # Reject unencodable requests before opening any control socket.
    hold = args.hold_seconds
    if not math.isfinite(hold) or not 0 < hold <= MAX_HOLD_SECONDS:
        raise ValueError(f"--hold-seconds must be finite in (0, {MAX_HOLD_SECONDS:g}]")
    limits = limits_from_args(args, allow_execute=True)

    sock, controller, collector = open_control_socket(profile)
    try:
        report(f"query original {controller[0]}:{controller[1]} -> {collector[0]}:{collector[1]}")
        cam, values = exchange(sock, limits,
                               query_command(limits.stream_id, bridge.MessageIds().take()))
    finally:
        sock.close()
    if cam & bridge.CAM_ERRORS:
        raise bridge.CommandRejected("collector rejected the initial state query")
    original_hz = values["rf_freq_hz"]
    report(f"original ACK cam=0x{cam:08X} {describe(values)}")

    failure = None
    restore_error = None
    may_have_applied = False
    try:
        datagram = tune_command(limits.stream_id, bridge.MessageIds().take(), rf_hz)
        parse_and_check(datagram, limits)
        report(f"tune only RF to {rf_hz / 1e6:g} MHz")
        exec_sock, _, _ = open_control_socket(profile)
        try:
            may_have_applied = True
            cam, values = exchange(exec_sock, limits, datagram)
        finally:
            exec_sock.close()
        if cam & bridge.CAM_ERRORS:
            may_have_applied = False
            raise bridge.CommandRejected(
                "collector rejected the tune; the original frequency is unchanged")
        report(f"tune ACK cam=0x{cam:08X} {describe(values)}")
        report(f"holding {hold:g}s, then restoring the original {original_hz / 1e6:g} MHz")
        time.sleep(hold)
    except BaseException as exc:
        failure = exc
    finally:
        if may_have_applied:
            report(f"restore original RF {original_hz / 1e6:g} MHz "
                  "(distinct command, fresh socket and IDs)")
            try:
                cam = restore_frequency(profile, original_hz, limits)
            except (bridge.BridgeError, OSError) as exc:
                restore_error = exc
            else:
                report(f"restore ACK cam=0x{cam:08X} {describe({'rf_freq_hz': original_hz})}")

    if restore_error is not None:
        report(f"RESTORE FAILED: {restore_error}; collector RF is not confirmed")
        return 2
    if failure is not None:
        report(f"tune failed: {failure}")
        return 1
    return 0

def run_tune(args, profile=LAN_PROFILE):
    with SessionLock(profile):
        return _run_tune_session(args, profile)



def _serve_relay_session(listener, limits, profile=LAN_PROFILE, max_datagrams=None):
    """Relay validated loopback plugin datagrams through the LAN control socket.

    The listener is supplied already bound so tests can use an ephemeral loopback
    port; production binds it in `run_relay`. Peer pinning, query-only policy and
    rejection ACKs come from `LocalBridge`; IDs and ACK checks come from the LAN
    transport.
    """
    sock, controller, collector = open_control_socket(profile)
    try:
        bridge.diagnostic(f"relay {listener.getsockname()[0]}:{listener.getsockname()[1]}"
                          f" -> {controller[0]}:{controller[1]}"
                          f" -> {collector[0]}:{collector[1]}"
                          f" (execute={'on' if limits.allow_execute else 'off'})")
        bridge.diagnostic("loopback peering is not authentication; keep this host isolated")
        return bridge.LocalBridge(listener, LanTransport(sock, limits), limits).serve(max_datagrams)
    finally:
        sock.close()

def serve_relay(listener, limits, profile=LAN_PROFILE, max_datagrams=None):
    with SessionLock(profile):
        return _serve_relay_session(listener, limits, profile, max_datagrams)



def run_relay(args, profile=LAN_PROFILE):
    require_trusted_lan(args, profile)
    limits = limits_from_args(args, allow_execute=args.allow_execute)
    if limits.allow_execute and not args.trusted_local_session:
        raise ValueError("relay execute requires --trusted-local-session; loopback peering "
                         "is not client authentication")
    listener_endpoint = bridge.loopback_endpoint(args.bind, args.ctrl_port, "relay listener")
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as listener:
        listener.bind(listener_endpoint)
        serve_relay(listener, limits, profile, args.max_datagrams)
    return 0


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    modes = parser.add_subparsers(dest="mode", required=True)

    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--trusted-lan", action="store_true",
                        help=f"acknowledge the authorized {AUTHORIZED_SUBNET} trusted LAN; required")
    common.add_argument("--timeout", type=float, default=3.0, help="bounded exchange timeout (s)")
    common.add_argument("--stream-id", type=lambda text: int(text, 0), default=DEFAULT_STREAM_ID)
    common.add_argument("--max-ignored", type=int, default=32)

    query = modes.add_parser("query", parents=[common],
                             help="read receiver state; query-state only")
    query.set_defaults(handler=run_query)

    tune = modes.add_parser("tune", parents=[common],
                            help="tune only RF frequency, hold finitely, restore the original")
    tune.add_argument("--frequency-mhz", type=float, required=True,
                      help=f"target RF frequency in MHz, e.g. {DEMO_FREQUENCY_MHZ} "
                           f"(authorized alternative {ALTERNATE_FREQUENCY_MHZ:g})")
    tune.add_argument("--hold-seconds", type=float, required=True,
                      help=f"explicit hold in (0, {MAX_HOLD_SECONDS:g}] before the restore")
    tune.add_argument("--allow-execute", action="store_true",
                      help="permit the RF execute commands; the collector must also allow them")
    tune.set_defaults(handler=run_tune)

    relay = modes.add_parser("relay", parents=[common],
                             help="optional loopback relay for the Soapy plugin")
    relay.add_argument("--bind", default=DEFAULT_RELAY_ADDR,
                       help="numeric loopback address the Soapy plugin sends to")
    relay.add_argument("--ctrl-port", type=int, default=DEFAULT_RELAY_PORT,
                       help="must equal the plugin's ctrl_port")
    relay.add_argument("--allow-execute", action="store_true",
                       help="permit mutations; must also be enabled on the collector")
    relay.add_argument("--trusted-local-session", action="store_true",
                       help="assert an OS-isolated trusted desktop session; loopback peering "
                            "is not authentication")
    relay.add_argument("--max-datagrams", type=int, default=None,
                       help="stop after N handled datagrams (tests only)")
    relay.set_defaults(handler=run_relay)
    return parser


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        return args.handler(args)
    except (bridge.BridgeError, OSError, ValueError) as exc:
        report(f"vrt LAN control failed: {exc}")
        return 1
    finally:
        bridge.diagnostic.flush()


if __name__ == "__main__":
    sys.exit(main())
