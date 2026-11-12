import io
import ipaddress
import queue
import socket
import struct
import subprocess
import sys
import threading
import unittest
from pathlib import Path
from unittest import mock

import vrt_control_bridge as bridge
import vrt_lan_control as lan

# The real profile is the fixed trusted-LAN pair. Tests inject the loopback
# subnet only, so no test can ever reach 192.168.0.0/24.
TEST_PROFILE = lan.LanProfile(
    subnet=ipaddress.IPv4Network("127.0.0.0/8"),
    controller=("127.0.0.1", lan.CONTROLLER_PORT),
    collector=("127.0.0.1", lan.COLLECTOR_PORT),
)
SOURCE = ("127.0.0.1", lan.CONTROLLER_PORT)
COLLECTOR = ("127.0.0.1", lan.COLLECTOR_PORT)
DEMO_HZ = 145_310_000.0
ORIGINAL_HZ = 915_000_000.0
APPLIED = bridge.CAM_EXECUTION_ACK | bridge.CAM_EXECUTED
# Never a bind value: the validator must reject this unspecified address.
UNSPECIFIED = str(ipaddress.IPv4Address(0))


def q20(hz):
    return lan.q20(hz)


def ack(message_id, stream_id=1, cam=bridge.CAM_QUERY_STATE, cif0=bridge.SUPPORTED,
        bandwidth=2e6, rf=ORIGINAL_HZ, gain=0.0, rate=250_000.0):
    """Independent ACK fixture: full supported readback for the wire message ID."""
    body = b""
    if cif0 & bridge.BW:
        body += struct.pack(">q", q20(bandwidth))
    if cif0 & bridge.RF:
        body += struct.pack(">q", q20(rf))
    if cif0 & bridge.GAIN:
        body += struct.pack(">hxx", round(gain * 128.0))
    if cif0 & bridge.RATE:
        body += struct.pack(">q", q20(rate))
    total = 20 + 8 + 4 + len(body)
    header = ((bridge.TYPE_COMMAND << bridge.TYPE_SHIFT) | bridge.ACK_FLAG
              | (3 << 22) | (1 << 20) | (total // 4))
    return (struct.pack(">5I", header, stream_id, 0, 0, 0)
            + struct.pack(">II", cam, message_id) + struct.pack(">I", cif0) + body)


def identity(datagram):
    found = bridge.ack_identity(datagram)
    if found is None:
        raise AssertionError("expected a well-formed VRT acknowledgement")
    return found


def parse_args(*argv):
    return lan.build_parser().parse_args(list(argv))


def run_async(handler, *argv):
    """Run one CLI handler with the injected loopback profile."""
    outcome = queue.Queue(maxsize=1)

    def target():
        output = io.StringIO()
        with mock.patch.object(lan, "report", side_effect=lambda text: output.write(str(text) + "\n")):
            try:
                result = handler(parse_args(*argv), TEST_PROFILE)
            except BaseException as exc:
                result = exc
        outcome.put((result, output.getvalue()))

    threading.Thread(target=target, daemon=True).start()
    return outcome


def finish(outcome, timeout=5.0):
    return outcome.get(timeout=timeout)


def require_fixed_ports():
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        probe.bind(SOURCE)
    except OSError:
        raise unittest.SkipTest(f"{SOURCE} is in use; fixed control port unverifiable") from None
    finally:
        probe.close()


def mock_collector():
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.bind(COLLECTOR)
    except OSError:
        sock.close()
        raise unittest.SkipTest(f"{COLLECTOR} is in use; mock collector cannot bind") from None
    sock.settimeout(2.0)
    return sock


class EndpointTests(unittest.TestCase):
    def test_default_profile_is_the_fixed_trusted_lan_pair(self):
        self.assertEqual(lan.LAN_PROFILE.subnet, ipaddress.IPv4Network("192.168.0.0/24"))
        self.assertEqual(lan.LAN_PROFILE.endpoints(),
                         (("192.168.0.15", 50000), ("192.168.0.55", 4991)))
        self.assertEqual(lan.DEMO_FREQUENCY_MHZ, 145.310)
        self.assertEqual(lan.MAX_HOLD_SECONDS, 120.0)

    def test_endpoints_must_be_numeric_unicast_inside_the_authorized_subnet(self):
        self.assertEqual(lan.lan_endpoint("127.0.0.1", 50000, "controller", TEST_PROFILE.subnet),
                         SOURCE)
        rejected = {
            "unspecified": (UNSPECIFIED, 50000),
            "broadcast": ("255.255.255.255", 50000),
            "subnet network": ("127.0.0.0", 50000),
            "subnet broadcast": ("127.255.255.255", 50000),
            "multicast": ("224.0.0.1", 50000),
            "out of subnet": ("192.168.1.15", 50000),
            "hostname": ("controller.local", 50000),
            "empty": ("", 50000),
            "ephemeral port": ("127.0.0.1", 0),
            "port too large": ("127.0.0.1", 70000),
            "negative port": ("127.0.0.1", -1),
        }
        for name, (host, port) in rejected.items():
            with self.subTest(name=name), self.assertRaises(ValueError):
                lan.lan_endpoint(host, port, "peer", TEST_PROFILE.subnet)
        with self.assertRaises(ValueError):
            lan.LanProfile(controller=("192.168.1.15", 50000)).endpoints()
        with self.assertRaises(ValueError):
            lan.lan_endpoint("127.0.0.1", 50000, "peer", ipaddress.IPv6Network("::/0"))
        with self.assertRaises(ValueError):
            lan.lan_endpoint("192.168.0.255", 50000, "peer", lan.AUTHORIZED_SUBNET)
        with self.assertRaises(ValueError):
            lan.q20(1e20)

    def test_query_is_query_only_and_tune_changes_rf_only(self):
        queried = bridge.parse_command(lan.query_command(1, 7))
        self.assertEqual(queried.cam, bridge.CAM_QUERY_STATE)
        self.assertEqual(bridge.check_command(queried, bridge.Limits()), 0)
        tuned = bridge.parse_command(lan.tune_command(1, 9, DEMO_HZ))
        self.assertEqual(tuned.cif0, bridge.RF)
        self.assertEqual(tuned.values, {"rf_freq_hz": DEMO_HZ})
        with self.assertRaises(bridge.CommandRejected):
            bridge.check_command(tuned, bridge.Limits())
        self.assertEqual(bridge.check_command(tuned, bridge.Limits(allow_execute=True)), 2)

    def test_cli_gates_precede_any_socket(self):
        with mock.patch.object(lan.socket, "socket", side_effect=AssertionError("bound socket")):
            with self.assertRaisesRegex(ValueError, "trusted-lan"):
                lan.run_query(parse_args("query"))
            with self.assertRaisesRegex(ValueError, "allow-execute"):
                lan.run_tune(parse_args("tune", "--trusted-lan", "--frequency-mhz", "145.31",
                                        "--hold-seconds", "1"))
            with self.assertRaisesRegex(ValueError, "frequency-mhz"):
                lan.run_tune(parse_args("tune", "--trusted-lan", "--allow-execute",
                                        "--frequency-mhz", "nan", "--hold-seconds", "1"))
            with self.assertRaisesRegex(ValueError, "hold-seconds"):
                lan.run_tune(parse_args("tune", "--trusted-lan", "--allow-execute",
                                        "--frequency-mhz", "145.31", "--hold-seconds", "121"))
            with self.assertRaisesRegex(ValueError, "trusted-local-session"):
                lan.run_relay(parse_args("relay", "--trusted-lan", "--allow-execute"))

    def test_session_mutex_excludes_another_windows_process(self):
        code = '''
import ipaddress, sys
sys.path.insert(0, sys.argv[1])
import vrt_lan_control as lan
p = lan.LanProfile(subnet=ipaddress.IPv4Network("127.0.0.0/8"),
                   controller=("127.0.0.1", 50000), collector=("127.0.0.1", 4991))
try:
    with lan.SessionLock(p):
        sys.exit(2)
except lan.SessionBusy:
    sys.exit(0)
'''
        with lan.SessionLock(TEST_PROFILE):
            result = subprocess.run([sys.executable, "-c", code, str(Path(lan.__file__).parent)],
                                    capture_output=True, timeout=5.0)
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        released = subprocess.run([sys.executable, "-c", code, str(Path(lan.__file__).parent)],
                                  capture_output=True, timeout=5.0)
        self.assertEqual(released.returncode, 2, released.stderr.decode(errors="replace"))


class QueryTests(unittest.TestCase):
    def setUp(self):
        require_fixed_ports()
        self.collector = mock_collector()
        self.addCleanup(self.collector.close)

    def test_default_query_uses_the_fixed_source_and_reports_the_readback(self):
        outcome = run_async(lan.run_query, "query", "--trusted-lan", "--timeout", "1")
        datagram, source = self.collector.recvfrom(65535)
        self.assertEqual(source, SOURCE)
        request = bridge.parse_command(datagram)
        self.assertEqual(request.cam, bridge.CAM_QUERY_STATE)
        self.assertEqual(request.stream_id, 1)
        self.collector.sendto(ack(request.message_id, rf=DEMO_HZ), source)
        result, output = finish(outcome)
        self.assertEqual(result, 0)
        self.assertIn("query ACK", output)
        self.assertIn("145.31MHz", output)

    def test_query_api_returns_precise_rf_when_all_diagnostics_are_dropped(self):
        def silently(args, profile):
            with mock.patch.object(lan, "report", return_value=None):
                return lan.query_frequency(args, profile)

        outcome = run_async(silently, "query", "--trusted-lan")
        datagram, source = self.collector.recvfrom(65535)
        request = bridge.parse_command(datagram)
        actual = 145_310_123.0
        self.collector.sendto(ack(request.message_id, rf=actual), source)
        result, output = finish(outcome)
        self.assertEqual(result, actual)
        self.assertEqual(output, "")
        self.assertIn("145.310123MHz", lan.describe({"rf_freq_hz": actual}))
        self.assertIn("145310123Hz", lan.describe({"rf_freq_hz": actual}))

    def test_stale_ack_is_ignored_and_the_matching_wire_id_wins(self):
        outcome = run_async(lan.run_query, "query", "--trusted-lan", "--timeout", "1")
        datagram, source = self.collector.recvfrom(65535)
        request = bridge.parse_command(datagram)
        self.collector.sendto(ack((request.message_id + 1) & 0xFFFFFFFF, rf=448e6), source)
        self.collector.sendto(ack(request.message_id, rf=DEMO_HZ), source)
        result, output = finish(outcome)
        self.assertEqual(result, 0)
        self.assertIn("145.31MHz", output)

    def test_missing_ack_times_out_once_without_a_retry(self):
        outcome = run_async(lan.run_query, "query", "--trusted-lan", "--timeout", "0.3")
        datagram, source = self.collector.recvfrom(65535)
        result, _ = finish(outcome)
        self.assertIsInstance(result, bridge.AckTimeout)
        self.collector.settimeout(0.5)
        with self.assertRaises(TimeoutError):
            self.collector.recvfrom(65535)

    def test_rejected_initial_query_cannot_report_success_or_issue_a_tune(self):
        cases = [(lan.run_query, ("query", "--trusted-lan")),
                 (lan.run_tune, ("tune", "--trusted-lan", "--allow-execute",
                                 "--frequency-mhz", "145.310", "--hold-seconds", "0.2"))]
        for handler, argv in cases:
            outcome = run_async(handler, *argv)
            datagram, source = self.collector.recvfrom(65535)
            self.collector.sendto(bridge.build_error_ack(bridge.parse_command(datagram)), source)
            result, _ = finish(outcome)
            self.assertIsInstance(result, bridge.CommandRejected)
            self.collector.settimeout(0.2)
            with self.assertRaises(TimeoutError):
                self.collector.recvfrom(65535)
            self.collector.settimeout(2.0)


class TuneTests(unittest.TestCase):
    def setUp(self):
        require_fixed_ports()
        self.collector = mock_collector()
        self.addCleanup(self.collector.close)

    def query_original(self):
        datagram, source = self.collector.recvfrom(65535)
        request = bridge.parse_command(datagram)
        self.collector.sendto(ack(request.message_id, rf=ORIGINAL_HZ), source)
        return request, source

    def test_tune_changes_rf_only_and_restores_the_original_after_the_hold(self):
        outcome = run_async(lan.run_tune, "tune", "--trusted-lan", "--allow-execute",
                            "--frequency-mhz", "145.310", "--hold-seconds", "0.2",
                            "--timeout", "1")
        original, source = self.query_original()
        tune, tune_source = self.collector.recvfrom(65535)
        self.assertEqual(tune_source, SOURCE)
        target = bridge.parse_command(tune)
        self.assertEqual(target.cif0, bridge.RF)
        self.assertEqual(q20(target.values["rf_freq_hz"]), q20(DEMO_HZ))
        self.collector.sendto(ack(target.message_id, cam=APPLIED, rf=DEMO_HZ), source)
        restore, _ = self.collector.recvfrom(65535)
        rollback = bridge.parse_command(restore)
        self.assertEqual(rollback.cif0, bridge.RF)
        self.assertEqual(q20(rollback.values["rf_freq_hz"]), q20(ORIGINAL_HZ))
        self.collector.sendto(ack(rollback.message_id, cam=APPLIED, rf=ORIGINAL_HZ), source)
        result, output = finish(outcome)
        self.assertEqual(result, 0)
        self.assertIn("tune ACK", output)
        self.assertIn("145.31MHz", output)
        self.assertIn("restore ACK", output)
        self.assertEqual(len({original.message_id, target.message_id,
                              rollback.message_id}), 3, "each exchange needs a fresh wire ID")

    def test_uncertain_execute_is_never_retried_and_restore_is_a_distinct_command(self):
        outcome = run_async(lan.run_tune, "tune", "--trusted-lan", "--allow-execute",
                            "--frequency-mhz", "145.310", "--hold-seconds", "5",
                            "--timeout", "0.3")
        self.query_original()
        tune, _ = self.collector.recvfrom(65535)
        target = bridge.parse_command(tune)
        # The target ACK never arrives: the client must proceed to a rollback
        # execute with the original frequency instead of resending the tune.
        restore, restore_source = self.collector.recvfrom(65535)
        self.assertEqual(restore_source, SOURCE)
        rollback = bridge.parse_command(restore)
        self.assertEqual(q20(rollback.values["rf_freq_hz"]), q20(ORIGINAL_HZ))
        self.assertNotEqual(rollback.message_id, target.message_id)
        self.collector.sendto(ack(rollback.message_id, cam=APPLIED, rf=ORIGINAL_HZ),
                              restore_source)
        result, _ = finish(outcome)
        self.assertEqual(result, 1)
        self.collector.settimeout(0.5)
        with self.assertRaises(TimeoutError):
            self.collector.recvfrom(65535)

    def test_rejected_execute_fails_without_a_needless_rollback_execute(self):
        outcome = run_async(lan.run_tune, "tune", "--trusted-lan", "--allow-execute",
                            "--frequency-mhz", "145.310", "--hold-seconds", "1",
                            "--timeout", "1")
        self.query_original()
        tune, source = self.collector.recvfrom(65535)
        self.collector.sendto(bridge.build_error_ack(bridge.parse_command(tune)), source)
        result, output = finish(outcome)
        self.assertEqual(result, 1)
        self.assertNotIn("restore ACK", output)
        self.collector.settimeout(0.5)
        with self.assertRaises(TimeoutError):
            self.collector.recvfrom(65535)

    def test_failed_restore_is_reported_nonzero_and_not_retried(self):
        outcome = run_async(lan.run_tune, "tune", "--trusted-lan", "--allow-execute",
                            "--frequency-mhz", "145.310", "--hold-seconds", "0.1",
                            "--timeout", "0.3")
        self.query_original()
        tune, source = self.collector.recvfrom(65535)
        target = bridge.parse_command(tune)
        self.collector.sendto(ack(target.message_id, cam=APPLIED, rf=DEMO_HZ), source)
        restore, _ = self.collector.recvfrom(65535)
        self.assertEqual(q20(bridge.parse_command(restore).values["rf_freq_hz"]), q20(ORIGINAL_HZ))
        result, _ = finish(outcome)
        self.assertEqual(result, 2)
        self.collector.settimeout(0.5)
        with self.assertRaises(TimeoutError):
            self.collector.recvfrom(65535)

    def test_hold_blocks_other_query_and_relay_until_original_is_restored(self):
        outcome = run_async(lan.run_tune, "tune", "--trusted-lan", "--allow-execute",
                            "--frequency-mhz", "145.310", "--hold-seconds", "0.6")
        self.query_original()
        packet, source = self.collector.recvfrom(65535)
        wire = bridge.parse_command(packet)
        self.collector.sendto(ack(wire.message_id, cam=APPLIED, rf=DEMO_HZ), source)
        with self.assertRaises(lan.SessionBusy):
            lan.query_state(parse_args("query", "--trusted-lan"), TEST_PROFILE)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as listener:
            listener.bind(("127.0.0.1", 0))
            with self.assertRaises(lan.SessionBusy):
                lan.serve_relay(listener, bridge.Limits(), TEST_PROFILE, 1)
        restored, source = self.collector.recvfrom(65535)
        request = bridge.parse_command(restored)
        self.assertEqual(request.values, {"rf_freq_hz": ORIGINAL_HZ})
        self.collector.sendto(ack(request.message_id, cam=APPLIED, rf=ORIGINAL_HZ), source)
        result, _ = finish(outcome)
        self.assertEqual(result, 0)

    def test_restore_reports_a_non_round_original_without_hiding_readback(self):
        original_hz = 915_000_123.0
        outcome = run_async(lan.run_tune, "tune", "--trusted-lan", "--allow-execute",
                            "--frequency-mhz", "145.310", "--hold-seconds", "0.05")
        packet, source = self.collector.recvfrom(65535)
        request = bridge.parse_command(packet)
        self.collector.sendto(ack(request.message_id, rf=original_hz), source)
        packet, source = self.collector.recvfrom(65535)
        target = bridge.parse_command(packet)
        self.collector.sendto(ack(target.message_id, cam=APPLIED, rf=DEMO_HZ), source)
        packet, source = self.collector.recvfrom(65535)
        restore = bridge.parse_command(packet)
        self.assertEqual(restore.values, {"rf_freq_hz": original_hz})
        self.collector.sendto(ack(restore.message_id, cam=APPLIED, rf=original_hz), source)
        result, output = finish(outcome)
        self.assertEqual(result, 0)
        restored_line = next(line for line in output.splitlines() if "restore ACK" in line)
        self.assertIn("915.000123MHz", restored_line)
        self.assertIn("915000123Hz", restored_line)


class RelayTests(unittest.TestCase):
    def setUp(self):
        require_fixed_ports()
        self.collector = mock_collector()
        self.addCleanup(self.collector.close)
        self.listener = self.loopback_socket()
        self.plugin = self.loopback_socket()

    def loopback_socket(self):
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.bind(("127.0.0.1", 0))
        sock.settimeout(2.0)
        self.addCleanup(sock.close)
        return sock

    def serve(self, limits, max_datagrams=1):
        outcome = queue.Queue(maxsize=1)

        def target():
            try:
                outcome.put(lan.serve_relay(self.listener, limits, TEST_PROFILE, max_datagrams))
            except BaseException as exc:
                outcome.put(exc)

        threading.Thread(target=target, daemon=True).start()
        return outcome

    def test_loopback_relay_round_trip_keeps_client_id_and_hides_the_wire_id(self):
        outcome = self.serve(bridge.Limits(timeout=1.0))
        request = lan.query_command(1, 7)
        self.plugin.sendto(request, self.listener.getsockname())
        forwarded, source = self.collector.recvfrom(65535)
        self.assertEqual(source, SOURCE)
        wire = bridge.parse_command(forwarded)
        self.assertNotEqual(wire.message_id, 7, "collector must see a fresh wire ID")
        self.collector.sendto(ack(wire.message_id, rf=DEMO_HZ), source)
        reply, reply_source = self.plugin.recvfrom(65535)
        self.assertEqual(reply_source, self.listener.getsockname())
        returned = bridge.parse_command(reply)
        self.assertEqual(returned.message_id, 7, "client ID must be restored")
        self.assertEqual(bridge.check_relayed_ack(reply, bridge.parse_command(request)),
                         bridge.CAM_QUERY_STATE)
        self.assertEqual(outcome.get(timeout=5.0), 1)

    def test_query_only_relay_denies_the_100mhz_startup_execute(self):
        outcome = self.serve(bridge.Limits(timeout=1.0))
        self.plugin.sendto(lan.tune_command(1, 11, 100e6), self.listener.getsockname())
        reply, source = self.plugin.recvfrom(65535)
        self.assertEqual(source, self.listener.getsockname())
        stream, message, cam = identity(reply)
        self.assertEqual((stream, message), (1, 11))
        self.assertTrue(cam & bridge.CAM_ERRORS)
        self.assertFalse(cam & bridge.CAM_EXECUTED)
        self.assertEqual(outcome.get(timeout=5.0), 1)
        self.collector.settimeout(0.5)
        with self.assertRaises(TimeoutError):
            self.collector.recvfrom(65535)

    def test_allowed_relay_execute_returns_the_applied_ack(self):
        outcome = self.serve(bridge.Limits(timeout=1.0, allow_execute=True))
        self.plugin.sendto(lan.tune_command(1, 11, DEMO_HZ), self.listener.getsockname())
        forwarded, source = self.collector.recvfrom(65535)
        wire = bridge.parse_command(forwarded)
        self.assertEqual(q20(wire.values["rf_freq_hz"]), q20(DEMO_HZ))
        self.collector.sendto(ack(wire.message_id, cam=APPLIED, rf=DEMO_HZ), source)
        reply, _ = self.plugin.recvfrom(65535)
        stream, message, cam = identity(reply)
        self.assertEqual((stream, message), (1, 11))
        self.assertEqual(cam & (bridge.CAM_EXECUTED | bridge.CAM_ERRORS), bridge.CAM_EXECUTED)
        self.assertEqual(outcome.get(timeout=5.0), 1)

    def test_armed_relay_denies_rate_gain_bandwidth_and_mixed_executes(self):
        cases = [(bridge.RATE, struct.pack(">q", q20(250_000))),
                 (bridge.GAIN, struct.pack(">I", 1280)),
                 (bridge.BW, struct.pack(">q", q20(250_000))),
                 (bridge.RF | bridge.RATE, struct.pack(">qq", q20(DEMO_HZ), q20(250_000)))]
        outcome = self.serve(bridge.Limits(allow_execute=True), len(cases))
        for message, (cif0, fields) in enumerate(cases):
            command = lan.build_command(1, message, 2 << 23, cif0, fields)
            self.plugin.sendto(command, self.listener.getsockname())
            reply, _ = self.plugin.recvfrom(65535)
            self.assertEqual(identity(reply)[1], message)
            self.assertTrue(identity(reply)[2] & bridge.CAM_ERRORS)
            self.assertFalse(identity(reply)[2] & bridge.CAM_EXECUTED)
        self.assertEqual(outcome.get(timeout=5.0), len(cases))
        self.collector.settimeout(0.2)
        with self.assertRaises(TimeoutError):
            self.collector.recvfrom(65535)


if __name__ == "__main__":
    unittest.main()
