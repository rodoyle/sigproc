"""Independent decoder vectors; no encoder or live radio is required."""
import struct
import unittest

from vrt_listener import decode


class DecodeTests(unittest.TestCase):
    def test_ci16_and_timestamp(self):
        packet = bytes.fromhex("10d70006 56495441 00000001 00000000 00000002 0001ffff")
        info = decode(packet)
        self.assertEqual(info["stream_id"], 0x56495441)
        self.assertEqual((info["sequence"], info["seconds"], info["fractional_samples"]), (7, 1, 2))
        self.assertEqual(info.get("first_iq"), (1, -1))
        # Valid payload changes must not fail a framing check.
        self.assertEqual(decode(packet[:-4] + bytes.fromhex("7fff8000")).get("first_iq"), (32767, -32768))

    def test_context(self):
        packet = struct.pack("!15I", 0x40D7000F, 17, 1, 0, 2, 0xA8A08000,
                             0xF4, 0x24000000, 0x5F5E, 0x10000000,
                             0x500, 0xF4, 0x24000000, 0x200003CF, 0)
        info = decode(packet)
        self.assertEqual((info.get("frequency_hz"), info.get("sample_rate_hz"), info.get("gain_db")), (100e6, 1e6, 10))
        self.assertEqual(info.get("bandwidth_hz"), 1e6)
        with self.assertRaises(ValueError):
            decode(packet[:-8] + bytes(8))  # Wrong CI16 format must fail.

    def test_length_and_header_flags(self):
        packet = bytes.fromhex("10d70006 56495441 00000001 00000000 00000002 0001ffff")
        for bad in [packet[:-1], packet[:3] + b"\x05" + packet[4:],
                    b"\x18" + packet[1:], b"\x10\xc7" + packet[2:]]:
            with self.subTest(packet=bad), self.assertRaises(ValueError):
                decode(bad)


if __name__ == "__main__":
    unittest.main()
