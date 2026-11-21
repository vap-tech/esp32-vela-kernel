import struct
import unittest

from flash_assets import FLASH_ADDRESS, PIXELS, attach, bundle, verify_bundle
from image import BINDING, FLASH_BASE, make_rom, pack
from verify_image import verify


def solid(pixel):
    count, remainder = divmod(PIXELS, 65535)
    return struct.pack("<HH", 65535, pixel) * count + struct.pack("<HH", remainder, pixel)


class FlashAssetTests(unittest.TestCase):
    def firmware(self):
        header = bytearray(24)
        header[0] = 0xE9
        header[23] = 1
        struct.pack_into("<I", header, 4, 0x30100000)
        struct.pack_into("<H", header, 12, 18)
        return pack(make_rom(header, [(0x30100000, BINDING), (0x4FF40000, b"abcd")]))

    def test_asset_image_and_corruption(self):
        firmware = self.firmware()
        assets = bundle([solid(0), solid(65535)])
        combined = attach(firmware, assets, FLASH_BASE)
        self.assertEqual(verify(combined, require_assets=True), (16, 4))
        with self.assertRaises(ValueError):
            verify(firmware, require_assets=True)
        at = FLASH_ADDRESS - FLASH_BASE
        for offset in [len(firmware) + 1, at, at + 16, at + 40, at + 64, len(combined) - 1]:
            damaged = bytearray(combined)
            damaged[offset] ^= 1
            with self.subTest(offset=offset), self.assertRaises(ValueError):
                verify(damaged)
        for cut in [at, at + 63, len(combined) - 4]:
            with self.subTest(cut=cut), self.assertRaises(ValueError):
                verify(combined[:cut])

    def test_reject_wrong_pixel_count_and_overlap(self):
        for stream in [b"", b"abc", struct.pack("<HH", 0, 0), struct.pack("<HH", 1, 0)]:
            with self.subTest(stream=stream), self.assertRaises(ValueError):
                bundle([stream, solid(0)])
        assets = bundle([solid(0), solid(1)])
        with self.assertRaises(ValueError):
            verify_bundle(assets + b"\0")
        with self.assertRaises(ValueError):
            attach(bytes(FLASH_ADDRESS - FLASH_BASE + 1), assets, FLASH_BASE)
