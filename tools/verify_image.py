"""Validate final Flash image, including all integrity fields and padding."""
from pathlib import Path
import struct
import sys
import zlib
from image import parse_rom, FLASH_BASE, MANIFEST, PAYLOAD


def verify(image):
    if len(image) < MANIFEST - FLASH_BASE + 64:
        raise ValueError('truncated firmware')
    if image[0] != 0xe9 or image[1] != 1:
        raise ValueError('expected single ROM segment')
    address, size = struct.unpack_from('<II', image, 24)
    end = ((32 + size) | 15) + 1 + (32 if image[23] else 0)
    header, segments = parse_rom(image[:end])
    if address != 0x30100000 or not 0 < size <= 8192:
        raise ValueError('SPM range')
    if struct.unpack_from('<I', header, 4)[0] != address:
        raise ValueError('ROM entry')
    at = MANIFEST - FLASH_BASE
    if end > at or image[end:at] != b'\xff' * (at - end):
        raise ValueError('unexpected padding')
    magic, version, dest, length, crc, source, flags, reserved = struct.unpack_from('<8s6I32s', image, at)
    if (magic, version, dest, source, flags, reserved) != (b'P4UPPER1', 1, 0x4ff40000, PAYLOAD, 0, bytes(32)):
        raise ValueError('manifest fields')
    if not length or length % 4 or dest + length > 0x4ffac000:
        raise ValueError('payload range')
    data = image[at+64:]
    if len(data) != length or zlib.crc32(data) != crc:
        raise ValueError('payload CRC/length')
    spm = segments[0][1]
    if spm.count(b'P4BIND01') != 1:
        raise ValueError('binding marker')
    bound, inverse = struct.unpack_from('<II', spm, spm.index(b'P4BIND01') + 8)
    if bound != crc or inverse != crc ^ 0xffffffff:
        raise ValueError('binding CRC')
    return size, length

if __name__ == '__main__':
    print('Final image verified: SPM=%d B, payload=%d B' % verify(Path(sys.argv[1]).read_bytes()))
