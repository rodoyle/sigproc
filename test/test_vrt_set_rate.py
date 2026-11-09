import struct
import unittest

import vrt_set_rate as client


def ack(stream=1, message=123, cam=(1 << 19) | (1 << 10), rate=250_000):
    header = (6 << 28) | (1 << 26) | (3 << 22) | (1 << 20) | 15
    return (struct.pack(">IIIII", header, stream, 0, 0, 0)
            + struct.pack(">IIIqqIq", cam, message, 0x28a00000,
                          2_000_000 * client.SCALE, 915_000_000 * client.SCALE,
                          0, rate * client.SCALE))


class RateClientTests(unittest.TestCase):
    def test_command_wire_format(self):
        packet = client.request(1, 250_000, 123)
        self.assertEqual(struct.unpack(">IIIIIq", packet),
                         ((6 << 28) | 7, 1, (2 << 23) | (1 << 19), 123,
                          1 << 21, 250_000 * client.SCALE))

    def test_readback_and_rejections(self):
        self.assertEqual(client.parse_ack(ack(), 1, 123), (250_000, False))
        self.assertEqual(client.parse_ack(ack(cam=(1 << 10) | (1 << 11)), 1, 123), (250_000, True))
        for data in (ack(stream=2), ack(message=124), ack(cam=1 << 16), ack()[:-1]):
            with self.subTest(data=data), self.assertRaises(ValueError):
                client.parse_ack(data, 1, 123)
        for rate in (0, -1, float("nan"), float("inf"), 1e30, 1e-20):
            with self.subTest(rate=rate), self.assertRaises(ValueError):
                client.request(1, rate, 123)


if __name__ == "__main__":
    unittest.main()
