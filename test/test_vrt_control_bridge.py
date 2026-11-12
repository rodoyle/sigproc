import io
import os
import queue
import socket
import struct
import subprocess
import sys
import threading
import unittest
from unittest import mock

import vrt_control_bridge as bridge

BRIDGE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "vrt_control_bridge.py")
Q20 = bridge.SCALE
SOURCE_PORT = 50000


def q20(value):
    return round(value * Q20)


def command(message_id=7, stream_id=1, cam=bridge.ACTION_EXECUTE << 23,
            cif0=bridge.RF | bridge.RATE, fields=b"", controllee_id=None):
    ids = b"" if controllee_id is None else struct.pack(">I", controllee_id)
    if controllee_id is not None:
        cam |= 1 << 31
    total = 20 + 8 + len(ids) + 4 + len(fields)
    header = (6 << 28) | (3 << 22) | (1 << 20) | (total // 4)
    return (struct.pack(">5I", header, stream_id, 0, 0, 0)
            + struct.pack(">II", cam, message_id) + ids
            + struct.pack(">I", cif0) + fields)


def frequency_execute(mhz, message_id=7, stream_id=1, controllee_id=None):
    return command(message_id=message_id, stream_id=stream_id, cif0=bridge.RF,
                   fields=struct.pack(">q", q20(mhz * 1e6)),
                   controllee_id=controllee_id)


def query(message_id=11, stream_id=1, controllee_id=None):
    body = struct.pack(">q", q20(2e6)) + struct.pack(">q", q20(915e6))
    body += struct.pack(">hxx", 0) + struct.pack(">q", q20(250_000))
    return command(message_id=message_id, stream_id=stream_id, cam=bridge.CAM_QUERY_STATE,
                   cif0=bridge.SUPPORTED, fields=body, controllee_id=controllee_id)


def ack(message_id=11, stream_id=1, cam=bridge.CAM_QUERY_STATE,
        cif0=bridge.SUPPORTED, fields=None, controllee_id=None):
    if fields is None:
        fields = struct.pack(">q", q20(2e6)) + struct.pack(">q", q20(915e6))
        fields += struct.pack(">hxx", 0) + struct.pack(">q", q20(250_000))
    ids = b"" if controllee_id is None else struct.pack(">I", controllee_id)
    if controllee_id is not None:
        cam |= 1 << 31
    total = 20 + 8 + 4 + len(fields) + len(ids)
    header = (6 << 28) | bridge.ACK_FLAG | (3 << 22) | (1 << 20) | (total // 4)
    return (struct.pack(">5I", header, stream_id, 0, 0, 0)
            + struct.pack(">II", cam, message_id) + ids + struct.pack(">I", cif0) + fields)


def identity(datagram):
    found = bridge.ack_identity(datagram)
    if found is None:
        raise AssertionError("expected a well-formed VRT acknowledgement")
    return found


class HungStream:
    """Pipe substitute whose read never returns: models a stuck SSH transport."""

    def __init__(self):
        self._release = threading.Event()

    def read1(self, size: int) -> bytes:
        self._release.wait()
        return b""

    read = read1

    def close(self):
        self._release.set()


class FakeProcess:
    def __init__(self, stdout=None, stdin=None):
        self.stdin = stdin if stdin is not None else io.BytesIO()
        self.stdout = stdout if stdout is not None else io.BytesIO()
        self.returncode = None
        self.terminated = False
        self.killed = False

    def poll(self):
        return self.returncode

    def terminate(self):
        self.terminated = True
        self.returncode = -15

    def kill(self):
        self.killed = True
        self.returncode = -9

    def wait(self, timeout=None):
        return self.returncode


def udp():
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("127.0.0.1", 0))
    sock.settimeout(2.0)
    return sock


class FramingTests(unittest.TestCase):
    def test_roundtrip_uses_exact_four_byte_big_endian_length(self):
        payload = query()
        sink = io.BytesIO()
        bridge.write_frame(sink, payload)
        self.assertEqual(sink.getvalue(), struct.pack(">I", len(payload)) + payload)
        self.assertEqual(bridge.FrameReader(io.BytesIO(sink.getvalue())).read_frame(1.0), payload)

    def test_oversize_zero_and_truncation_are_rejected(self):
        for prefix in (b"\x00\x00\x00\x00", struct.pack(">I", bridge.MAX_FRAME + 1)):
            with self.subTest(prefix=prefix), self.assertRaises(bridge.FrameProtocolError):
                bridge.FrameReader(io.BytesIO(prefix)).read_frame(1.0)
        with self.assertRaises(bridge.FrameProtocolError):
            bridge.pack_frame(b"\x00" * (bridge.MAX_FRAME + 1))
        for truncated in (b"\x00\x00", b"\x00\x00\x00\x08", b"\x00\x00\x00\x08abc"):
            with self.subTest(truncated=truncated), self.assertRaises(bridge.FrameTruncated):
                bridge.FrameReader(io.BytesIO(truncated)).read_frame(1.0)

    def test_orderly_eof_is_distinct_from_truncation(self):
        self.assertIsNone(bridge.FrameReader(io.BytesIO()).read_frame(1.0))
        reader = bridge.FrameReader(io.BytesIO(bridge.pack_frame(b"ok")))
        self.assertEqual(reader.read_frame(1.0), b"ok")
        self.assertIsNone(reader.read_frame(1.0))

    def test_hung_transport_times_out_once_and_is_cleaned_up(self):
        hung = HungStream()
        process = FakeProcess(stdout=hung)
        transport = bridge.SshTransport(process)
        payload = frequency_execute(100)
        try:
            with self.assertRaises(bridge.TransportTimeout):
                transport.exchange(payload, 0.2)
        finally:
            forwarded = process.stdin.getvalue()
            transport.close(force=True)
            hung.close()
        # Exactly one request frame was written: a timeout never resends.
        self.assertEqual(forwarded, bridge.pack_frame(payload))
        self.assertTrue(process.terminated)

    def test_partial_frame_deadline_applies_even_when_idle_is_allowed(self):
        class FirstByteThenHung(HungStream):
            def __init__(self):
                super().__init__()
                self.first = True

            def read1(self, size):
                if self.first:
                    self.first = False
                    return b"\x00"
                return super().read1(size)

        stream = FirstByteThenHung()
        reader = bridge.FrameReader(stream)
        try:
            with self.assertRaises(bridge.TransportTimeout):
                reader.read_frame(timeout=0.2, allow_idle=True)
        finally:
            reader.close()
            stream.close()

    def test_stalled_write_has_a_deadline_and_is_never_retried(self):
        class StalledSink:
            def __init__(self):
                self.release = threading.Event()
                self.entered = threading.Event()
                self.writes = 0

            def write(self, payload):
                self.writes += 1
                self.entered.set()
                self.release.wait()
                return len(payload)

            def flush(self):
                return None

        sink = StalledSink()
        try:
            with self.assertRaises(bridge.TransportTimeout):
                bridge.write_frame(sink, query(), timeout=0.2)
            self.assertTrue(sink.entered.is_set())
            self.assertEqual(sink.writes, 1)
        finally:
            sink.release.set()


class CommandProfileTests(unittest.TestCase):
    def test_q20_fields_ids_and_ack_identity(self):
        parsed = bridge.parse_command(frequency_execute(915, message_id=9, controllee_id=1))
        self.assertEqual(parsed.stream_id, 1)
        self.assertEqual(parsed.message_id, 9)
        self.assertEqual(parsed.controllee_id, 1)
        self.assertEqual(parsed.values, {"rf_freq_hz": 915e6})
        self.assertEqual(bridge.check_command(parsed, bridge.Limits(allow_execute=True)), 2)
        self.assertEqual(identity(ack(9))[1], 9)

    def test_query_is_the_default_and_execute_needs_both_ends(self):
        parsed = bridge.parse_command(query())
        self.assertEqual(bridge.check_command(parsed, bridge.Limits()), 0)
        with self.assertRaises(bridge.CommandRejected):
            bridge.check_command(bridge.parse_command(frequency_execute(100)), bridge.Limits())
        allowed = frequency_execute(100, message_id=3)
        self.assertEqual(bridge.check_command(bridge.parse_command(allowed),
                                             bridge.Limits(allow_execute=True)), 2)
        # Denial echoes identity with the error bit and never a success bit.
        denied = bridge.build_error_ack(bridge.parse_command(allowed))
        stream, message, cam = identity(denied)
        self.assertEqual((stream, message), (1, 3))
        self.assertTrue(cam & bridge.CAM_ERRORS)
        self.assertFalse(cam & bridge.CAM_EXECUTED)
        self.assertEqual(cam & bridge.CAM_EXECUTION_ACK, bridge.CAM_EXECUTION_ACK)
        parsed_denial = bridge.parse_command(denied)
        self.assertEqual(parsed_denial.cif0, 0, "policy denial has no receiver readback")
        self.assertEqual(parsed_denial.values, {})
        self.assertEqual(parsed_denial.body, b"")

    def test_profile_rejections_and_malformed_datagrams(self):
        limits = bridge.Limits()
        rejects = {
            "stream": frequency_execute(915, stream_id=2),
            "controllee": frequency_execute(915, controllee_id=2),
            "contradictory": command(cam=bridge.CAM_QUERY_STATE | (2 << 23), cif0=bridge.RF,
                                     fields=struct.pack(">q", q20(915e6))),
            "no fields": command(cif0=0),
            "unknown fields": command(cif0=1 << 5),
            "reserved cam": command(cam=(2 << 23) | (1 << 30), cif0=0),
        }
        for name, datagram in rejects.items():
            with self.subTest(name=name), self.assertRaises(bridge.CommandRejected):
                bridge.check_command(bridge.parse_command(datagram), limits)
        words = bytearray(frequency_execute(915))
        header = struct.unpack_from(">I", words)[0] | bridge.ACK_FLAG
        words[0:4] = struct.pack(">I", header)
        with self.assertRaises(bridge.CommandRejected):
            bridge.check_command(bridge.parse_command(bytes(words)), limits)
        for malformed in (b"\x00" * 4, struct.pack(">II", 4 << 28, 1),
                          frequency_execute(915)[:-4], frequency_execute(915) + b"\x00\x00\x00\x00"):
            with self.subTest(malformed=malformed), self.assertRaises(bridge.MalformedCommand):
                bridge.parse_command(malformed)
        with self.assertRaises(bridge.ProtocolError):
            bridge.check_relayed_ack(ack(99), bridge.parse_command(query(message_id=7)))
        with self.assertRaises(bridge.ProtocolError):
            bridge.check_relayed_ack(b"\x00" * 20, bridge.parse_command(query(message_id=7)))

    def test_query_ack_wire_fixture_and_execute_ack_are_not_interchangeable(self):
        requested = bridge.parse_command(query(message_id=11))
        # Independent literal words: Rust's query-state ACK has CAM bit18,
        # no execution ACK bit19 and no executed bit10. Do not use our ACK helper
        # to define that intentional cross-language protocol contract.
        fixture = struct.pack(">15I", 0x64D0000F, 1, 0, 0, 0, 0x00040000, 11,
                              0x28A00000, 0x1E8, 0x48000000, 0x3689C,
                              0xAC000000, 0x500, 0x1E8, 0x48000000)
        self.assertEqual(bridge.check_relayed_ack(fixture, requested), 0x00040000)
        execute = bridge.parse_command(frequency_execute(915, message_id=11))
        applied = ack(11, cam=bridge.CAM_EXECUTION_ACK | bridge.CAM_EXECUTED)
        self.assertEqual(bridge.check_relayed_ack(applied, execute),
                         bridge.CAM_EXECUTION_ACK | bridge.CAM_EXECUTED)
        for response, request in ((fixture, execute), (applied, requested),
                                  (ack(11, cam=bridge.CAM_QUERY_STATE | bridge.CAM_EXECUTED), requested),
                                  (ack(11, cam=bridge.CAM_EXECUTION_ACK), execute),
                                  (ack(11, cam=bridge.CAM_EXECUTION_ACK | bridge.CAM_EXECUTED
                                       | bridge.CAM_ERRORS), execute)):
            with self.subTest(cam=identity(response)[2]), self.assertRaises(bridge.ProtocolError):
                bridge.check_relayed_ack(response, request)

    def test_full_ack_layout_state_and_echoed_identifiers_are_required(self):
        requested = bridge.parse_command(query(message_id=11))
        header_only = struct.pack(">4I", 0x64000004, 1, bridge.CAM_QUERY_STATE, 11)
        truncated = bytearray(ack(11)[:-4])
        header = struct.unpack_from(">I", truncated)[0]
        struct.pack_into(">I", truncated, 0, (header & ~0xFFFF) | (len(truncated) // 4))
        malformed = [header_only, bytes(truncated), ack(11, cif0=0, fields=b""),
                     ack(11, cif0=1 << 5)]
        for response in malformed:
            with self.subTest(response=response), self.assertRaises(bridge.ProtocolError):
                bridge.check_relayed_ack(response, requested)
        identified = bridge.parse_command(query(message_id=11, controllee_id=1))
        self.assertEqual(bridge.check_relayed_ack(ack(11, controllee_id=1), identified),
                         bridge.CAM_QUERY_STATE | (1 << 31))
        for response in (ack(11), ack(11, controllee_id=2)):
            with self.assertRaises(bridge.ProtocolError):
                bridge.check_relayed_ack(response, identified)
        denial = bridge.build_error_ack(requested)
        self.assertEqual(bridge.check_relayed_ack(denial, requested),
                         bridge.CAM_EXECUTION_ACK | bridge.CAM_ERRORS)

    def test_execute_readback_mismatch_needs_partial_and_errors_cannot_query(self):
        requested = bridge.parse_command(frequency_execute(100, message_id=11))
        cam = bridge.CAM_EXECUTION_ACK | bridge.CAM_EXECUTED
        # The ACK fixture reports 915 MHz, not the requested 100 MHz.
        with self.assertRaises(bridge.ProtocolError):
            bridge.check_relayed_ack(ack(11, cam=cam), requested)
        self.assertEqual(bridge.check_relayed_ack(
            ack(11, cam=cam | bridge.CAM_PARTIAL_ACTION), requested),
            cam | bridge.CAM_PARTIAL_ACTION)
        queried = bridge.parse_command(query(message_id=11))
        for extra in (bridge.CAM_QUERY_STATE, bridge.CAM_PARTIAL_ACTION):
            bad = ack(11, cam=bridge.CAM_EXECUTION_ACK | bridge.CAM_ERRORS | extra,
                      cif0=0, fields=b"")
            with self.assertRaises(bridge.ProtocolError):
                bridge.check_relayed_ack(bad, queried)


class LocalBridgeTests(unittest.TestCase):
    def setUp(self):
        self.listener = udp()
        self.addCleanup(self.listener.close)

    def test_startup_100mhz_execute_is_denied_locally_without_reaching_node(self):
        plugin = udp()
        self.addCleanup(plugin.close)
        process = FakeProcess()
        transport = bridge.SshTransport(process)
        local = bridge.LocalBridge(self.listener, transport, bridge.Limits(timeout=1.0))
        hazard = frequency_execute(100, message_id=21)
        plugin.sendto(hazard, self.listener.getsockname())
        self.assertEqual(local.serve(max_datagrams=1), 1)
        reply, source = plugin.recvfrom(65535)
        forwarded = process.stdin.getvalue()
        transport.close()
        self.assertEqual(source, self.listener.getsockname())
        stream, message, cam = identity(reply)
        self.assertEqual((stream, message), (1, 21))
        self.assertTrue(cam & bridge.CAM_ERRORS)
        self.assertFalse(cam & bridge.CAM_EXECUTED)
        self.assertEqual(forwarded, b"", "denied execute was forwarded")

    def test_query_is_forwarded_once_and_ack_returns_from_the_listener_socket(self):
        plugin = udp()
        self.addCleanup(plugin.close)
        request = query(message_id=11)
        expected = ack(11)
        process = FakeProcess(stdout=io.BytesIO(bridge.pack_frame(expected)))
        transport = bridge.SshTransport(process)
        local = bridge.LocalBridge(self.listener, transport, bridge.Limits(timeout=1.0))
        plugin.sendto(request, self.listener.getsockname())
        self.assertEqual(local.serve(max_datagrams=1), 1)
        reply, source = plugin.recvfrom(65535)
        forwarded = process.stdin.getvalue()
        transport.close()
        self.assertEqual(reply, expected)
        self.assertEqual(source, self.listener.getsockname())
        self.assertEqual(forwarded, bridge.pack_frame(request))
        self.assertEqual(local.peer, plugin.getsockname())

    def test_second_client_is_not_cross_delivered_or_forwarded(self):
        first, second = udp(), udp()
        self.addCleanup(first.close)
        self.addCleanup(second.close)
        frames = bridge.pack_frame(ack(11)) + bridge.pack_frame(ack(12))
        process = FakeProcess(stdout=io.BytesIO(frames))
        transport = bridge.SshTransport(process)
        local = bridge.LocalBridge(self.listener, transport, bridge.Limits(timeout=1.0))
        first.sendto(query(message_id=11), self.listener.getsockname())
        local.serve(max_datagrams=1)
        first.recvfrom(65535)
        second.sendto(query(message_id=21), self.listener.getsockname())
        first.sendto(query(message_id=12), self.listener.getsockname())
        local.serve(max_datagrams=1)
        reply, _ = first.recvfrom(65535)
        forwarded = process.stdin.getvalue()
        transport.close()
        self.assertEqual(reply, ack(12))
        self.assertEqual(forwarded, bridge.pack_frame(query(message_id=11))
                         + bridge.pack_frame(query(message_id=12)))
        with self.assertRaises(TimeoutError):
            second.recvfrom(65535)

    def test_mismatched_ack_fails_closed_instead_of_relaying(self):
        plugin = udp()
        self.addCleanup(plugin.close)
        process = FakeProcess(stdout=io.BytesIO(bridge.pack_frame(ack(99))))
        transport = bridge.SshTransport(process)
        local = bridge.LocalBridge(self.listener, transport, bridge.Limits(timeout=1.0))
        plugin.sendto(query(message_id=11), self.listener.getsockname())
        with self.assertRaises(bridge.ProtocolError):
            local.serve(max_datagrams=1)
        transport.close()

    def test_malformed_sender_cannot_claim_peer_and_query_denial_is_relayed(self):
        malformed, plugin = udp(), udp()
        self.addCleanup(malformed.close)
        self.addCleanup(plugin.close)
        collector = udp()
        self.addCleanup(collector.close)
        remote = bridge.RemoteBridge(collector, None, None, bridge.Limits(stream_id=2))
        request = query(message_id=11)
        denied = remote.handle(request)
        process = FakeProcess(stdout=io.BytesIO(bridge.pack_frame(denied)))
        transport = bridge.SshTransport(process)
        self.addCleanup(transport.close, force=True)
        local = bridge.LocalBridge(self.listener, transport, bridge.Limits(timeout=1.0))
        malformed.sendto(b"not VRT", self.listener.getsockname())
        local.serve(max_datagrams=1)
        self.assertIsNone(local.peer)
        plugin.sendto(request, self.listener.getsockname())
        local.serve(max_datagrams=1)
        reply, source = plugin.recvfrom(65535)
        self.assertEqual(reply, denied)
        self.assertEqual(source, self.listener.getsockname())
        self.assertEqual(local.peer, plugin.getsockname())
        self.assertTrue(identity(reply)[2] & bridge.CAM_ERRORS)


class EndpointTests(unittest.TestCase):
    def test_only_numeric_loopback_endpoints_are_accepted(self):
        self.assertEqual(bridge.loopback_endpoint("127.0.0.1", 5599, "listener"),
                         ("127.0.0.1", 5599))
        for host in ("192.0.2.10", "10.0.0.1", "localhost", "::1", ""):
            with self.subTest(host=host), self.assertRaises(ValueError):
                bridge.loopback_endpoint(host, 5599, "listener")
        for port in (-1, 70000):
            with self.subTest(port=port), self.assertRaises(ValueError):
                bridge.loopback_endpoint("127.0.0.1", port, "listener")
        with self.assertRaises(ValueError):
            bridge.loopback_endpoint("127.0.0.1", 0, "collector")

    def test_ssh_argv_is_an_argv_list_with_forced_command_safe_options(self):
        argv = bridge.ssh_argv("vrt-control", "/home/operator/.ssh/vrt_ctrl", 5)
        self.assertEqual(argv[0], "ssh")
        self.assertIn("BatchMode=yes", argv)
        self.assertIn("StrictHostKeyChecking=yes", argv)
        self.assertIn("IdentitiesOnly=yes", argv)
        self.assertIn("IdentityAgent=none", argv)
        self.assertIn("ConnectTimeout=5", argv)
        self.assertIn("ClearAllForwardings=yes", argv)
        self.assertEqual(argv[-1], "vrt-control")
        self.assertEqual(argv[-3:-1], ["-i", "/home/operator/.ssh/vrt_ctrl"])
        for target in ("ops@node; rm -rf /", "-oProxyCommand=x", "user@host -v"):
            with self.subTest(target=target), self.assertRaises(ValueError):
                bridge.ssh_argv(target, "/key", 5)
        for timeout in (0, -1, float("nan"), 1e9):
            with self.subTest(timeout=timeout), self.assertRaises(ValueError):
                bridge.ssh_argv("vrt-control", "/key", timeout)

    def test_limits_are_validated_before_any_socket_or_process(self):
        for limits in (bridge.Limits(timeout=0), bridge.Limits(timeout=float("nan")),
                       bridge.Limits(timeout=61), bridge.Limits(stream_id=0),
                       bridge.Limits(max_ignored=-1)):
            with self.subTest(limits=limits), self.assertRaises(ValueError):
                limits.validate()

    def test_execute_requires_explicit_isolated_session_before_binding_or_ssh(self):
        args = bridge.build_parser().parse_args([
            "local", "--local-port", "5599", "--ssh-target", "vrt-control",
            "--identity", "/dedicated/key", "--allow-execute"])
        with (
            mock.patch.object(bridge.socket, "socket", side_effect=AssertionError("bound socket")),
            self.assertRaisesRegex(ValueError, "trusted-local-session"),
        ):
            bridge.run_local_mode(args)


class RemoteBridgeTests(unittest.TestCase):
    def spawn(self, collector, *extra, capture_stderr=False):
        argv = [sys.executable, BRIDGE, "remote", "--source-port", str(SOURCE_PORT),
                "--collector-port", str(collector.getsockname()[1]), *extra]
        process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE if capture_stderr else subprocess.DEVNULL,
                                   bufsize=0)
        def cleanup():
            if process.poll() is None:
                process.kill()
            process.wait(timeout=2.0)
            for stream in (process.stdin, process.stdout, process.stderr):
                if stream is not None:
                    stream.close()

        self.addCleanup(cleanup)
        return process

    def require_source_port(self):
        probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            probe.bind(("127.0.0.1", SOURCE_PORT))
        except OSError:
            self.skipTest(f"127.0.0.1:{SOURCE_PORT} is in use; fixed ACL port unverifiable")
        finally:
            probe.close()

    def test_local_helper_and_remote_helper_relay_one_query_end_to_end(self):
        self.require_source_port()
        collector = udp()
        self.addCleanup(collector.close)
        process = self.spawn(collector)
        transport = bridge.SshTransport(process)
        listener, plugin = udp(), udp()
        self.addCleanup(listener.close)
        self.addCleanup(plugin.close)
        local = bridge.LocalBridge(listener, transport, bridge.Limits(timeout=2.0))
        thread = threading.Thread(target=local.serve, args=(1,), daemon=True)
        thread.start()
        request = query(message_id=31)
        plugin.sendto(request, listener.getsockname())
        datagram, source = collector.recvfrom(65535)
        wire = bridge.parse_command(datagram)
        self.assertEqual(bridge.rewrite_message_id(datagram, wire, 31), request)
        self.assertNotEqual(wire.message_id, 31, "node correlation must not reuse client IDs")
        self.assertEqual(source, ("127.0.0.1", SOURCE_PORT))
        expected = ack(31)
        collector.sendto(ack((wire.message_id - 1) & 0xFFFFFFFF), source)
        collector.sendto(b"\x00" * 24, source)
        collector.sendto(ack(wire.message_id), source)
        reply, reply_source = plugin.recvfrom(65535)
        thread.join(5.0)
        self.assertFalse(thread.is_alive())
        self.assertEqual(reply, expected)
        self.assertEqual(reply_source, listener.getsockname())
        transport.close()
        self.assertEqual(process.wait(timeout=5.0), 0)

    def test_remote_helper_denies_execute_without_collector_traffic(self):
        self.require_source_port()
        collector = udp()
        self.addCleanup(collector.close)
        process = self.spawn(collector)
        transport = bridge.SshTransport(process)
        bridge.write_frame(process.stdin, frequency_execute(100, message_id=41))
        reply = transport.reader.read_frame(2.0)
        transport.close()
        stream, message, cam = identity(reply)
        self.assertEqual((stream, message), (1, 41))
        self.assertTrue(cam & bridge.CAM_ERRORS)
        self.assertFalse(cam & bridge.CAM_EXECUTED)
        with self.assertRaises(TimeoutError):
            collector.recvfrom(65535)
        self.assertEqual(process.wait(timeout=5.0), 0)

    def test_remote_timeout_sends_no_retry_and_exits_nonzero(self):
        self.require_source_port()
        collector = udp()
        self.addCleanup(collector.close)
        process = self.spawn(collector, "--timeout", "0.4")
        transport = bridge.SshTransport(process)
        self.addCleanup(transport.close, force=True)
        request = query(message_id=51)
        bridge.write_frame(process.stdin, request)
        datagram, source = collector.recvfrom(65535)
        wire = bridge.parse_command(datagram)
        self.assertEqual(bridge.rewrite_message_id(datagram, wire, 51), request)
        self.assertEqual(source, ("127.0.0.1", SOURCE_PORT))
        self.assertIsNone(transport.reader.read_frame(2.0))
        self.assertNotEqual(process.wait(timeout=5.0), 0)
        with self.assertRaises(TimeoutError):
            collector.recvfrom(65535)

    def test_new_session_ids_reject_delayed_execute_ack_with_reused_client_id(self):
        collector, node = udp(), udp()
        self.addCleanup(collector.close)
        self.addCleanup(node.close)
        node.connect(collector.getsockname())
        request = frequency_execute(915, message_id=11)
        old_wire_id = None
        for seed in (101, 202):
            with mock.patch.object(bridge.secrets, "randbits", return_value=seed):
                remote = bridge.RemoteBridge(node, None, None,
                                             bridge.Limits(timeout=0.2, allow_execute=True))
            outcomes = queue.Queue(maxsize=1)

            def exchange(endpoint=remote, sink=outcomes):
                try:
                    sink.put(endpoint.handle(request))
                except bridge.BridgeError as exc:
                    sink.put(exc)

            thread = threading.Thread(target=exchange, daemon=True)
            thread.start()
            forwarded, source = collector.recvfrom(65535)
            wire = bridge.parse_command(forwarded)
            self.assertEqual(wire.message_id, seed)
            if old_wire_id is not None:
                stale = bytearray(ack(old_wire_id,
                                     cam=bridge.CAM_EXECUTION_ACK | bridge.CAM_EXECUTED))
                struct.pack_into(">q", stale, 40, q20(100e6))
                collector.sendto(stale, source)
                collector.sendto(ack(wire.message_id,
                                     cam=bridge.CAM_EXECUTION_ACK | bridge.CAM_EXECUTED), source)
            thread.join(2.0)
            self.assertFalse(thread.is_alive())
            result = outcomes.get_nowait()
            if old_wire_id is None:
                self.assertIsInstance(result, bridge.AckTimeout)
            else:
                self.assertEqual(result, ack(11, cam=bridge.CAM_EXECUTION_ACK | bridge.CAM_EXECUTED))
            old_wire_id = wire.message_id

    def test_remote_exits_when_its_real_output_pipe_is_not_read(self):
        collector = udp()
        self.addCleanup(collector.close)
        process = self.spawn(collector, "--source-port", "0", "--timeout", "0.2")
        outcomes = queue.Queue(maxsize=1)

        def feed():
            try:
                bridge._write_all(process.stdin,
                                  bridge.pack_frame(frequency_execute(100)) * 20000)
            except (OSError, bridge.BridgeError) as exc:
                outcomes.put(exc)
            else:
                outcomes.put(None)

        thread = threading.Thread(target=feed, daemon=True)
        thread.start()
        self.assertNotEqual(process.wait(timeout=8.0), 0)
        thread.join(2.0)
        self.assertFalse(thread.is_alive(), "blocked stdin writer must unblock on teardown")
        self.assertIsInstance(outcomes.get_nowait(), OSError)

    def test_unread_stderr_cannot_block_denials_or_release_of_source_socket(self):
        collector = udp()
        self.addCleanup(collector.close)
        process = self.spawn(collector, "--source-port", "0", "--timeout", "1",
                             capture_stderr=True)
        transport = bridge.SshTransport(process)
        self.addCleanup(transport.close, force=True)
        # Drain ACKs but never stderr. Enough rejected requests fill ordinary
        # OS stderr pipes; diagnostic backpressure must not hold the UDP owner.
        for message in range(2048):
            reply = transport.exchange(frequency_execute(100, message_id=message), 2.0)
            self.assertEqual(identity(reply)[1], message)
            self.assertTrue(identity(reply)[2] & bridge.CAM_ERRORS)
        transport.close()
        self.assertEqual(process.wait(timeout=3.0), 0)


if __name__ == "__main__":
    unittest.main()
