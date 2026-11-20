import struct
import unittest
import zlib
from image import make_rom, parse_rom, pack, BINDING, MANIFEST, FLASH_BASE

class ImageTests(unittest.TestCase):
    def source(self, spm=0x30100000, upper=0x4ff40000, payload=b"abcd"):
        h = bytearray(24)
        h[0] = 0xe9
        h[23] = 1
        struct.pack_into("<I", h, 4, spm)
        struct.pack_into("<H", h, 12, 18)
        return make_rom(h, [(spm, BINDING), (upper, payload)])

    def test_split_integrity(self):
        data = pack(self.source())
        at = MANIFEST - FLASH_BASE
        fields = struct.unpack_from("<8s6I", data, at)
        self.assertEqual(fields, (b"P4UPPER1", 1, 0x4ff40000, 4, zlib.crc32(b"abcd"), 0x40040, 0))
        self.assertEqual(data[at+64:], b"abcd")
        # Parse the compact ROM container independently of FF padding.
        end = (24 + 8 + len(BINDING)) | 15
        _, segments = parse_rom(data[:end+1+32])
        self.assertEqual(len(segments), 1)
        crc, inv = struct.unpack_from("<II", segments[0][1], 8)
        self.assertEqual(crc, fields[4])
        self.assertEqual(crc ^ inv, 0xffffffff)

    def test_final_image_corruption(self):
        from verify_image import verify
        image = pack(self.source())
        self.assertEqual(verify(image), (16, 4))
        for at in [32, 64, MANIFEST-FLASH_BASE, len(image)-1]:
            bad = bytearray(image); bad[at] ^= 1
            with self.subTest(at=at), self.assertRaises(ValueError): verify(bad)
        with self.assertRaises(ValueError): verify(image[:-1])

    def test_invalid_ranges(self):
        for spm, upper, payload in [(0x30102000,0x4ff40000,b"abcd"), (0x30100000,0x4ff3fffc,b"abcd"), (0x30100000,0x4ffac000,b"abcd"), (0x30100000,0x4ff40004,b"abcd"), (0x30100000,0x4ff40000,b"a"*0x6c004)]:
            with self.subTest(spm=spm, upper=upper), self.assertRaises(ValueError):
                pack(self.source(spm, upper, payload))

    def test_corruption_and_truncation(self):
        original = self.source()
        for cut in [0, 23, 31, len(original)-1]:
            with self.assertRaises(ValueError): pack(original[:cut])
        for at in [32, len(original)-1]:
            bad = bytearray(original); bad[at] ^= 1
            with self.assertRaises(ValueError): pack(bad)

    def test_missing_binding(self):
        h, segments = parse_rom(self.source())
        with self.assertRaises(ValueError):
            pack(make_rom(h, [(segments[0][0], bytes(16)), segments[1]]))

if __name__ == "__main__": unittest.main()
