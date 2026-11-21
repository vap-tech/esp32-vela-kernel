"""Validate final Flash image, including all integrity fields and padding."""

from pathlib import Path
import struct
import sys
import zlib
from image import parse_rom, FLASH_BASE, MANIFEST, PAYLOAD
from flash_assets import FLASH_ADDRESS, FLASH_SIZE, verify_bundle


def verify(image, require_assets=False):
    if len(image) < MANIFEST - FLASH_BASE + 64:
        raise ValueError("truncated firmware")
    if image[0] != 0xE9 or image[1] != 1:
        raise ValueError("expected single ROM segment")
    address, size = struct.unpack_from("<II", image, 24)
    end = ((32 + size) | 15) + 1 + (32 if image[23] else 0)
    header, segments = parse_rom(image[:end])
    if address != 0x30100000 or not 0 < size <= 8192:
        raise ValueError("SPM range")
    if struct.unpack_from("<I", header, 4)[0] != address:
        raise ValueError("ROM entry")
    at = MANIFEST - FLASH_BASE
    if end > at or image[end:at] != b"\xff" * (at - end):
        raise ValueError("unexpected padding")
    magic, version, dest, length, crc, source, flags, reserved = struct.unpack_from(
        "<8s6I32s", image, at
    )
    if (magic, version, dest, source, flags, reserved) != (
        b"P4UPPER1",
        1,
        0x4FF40000,
        PAYLOAD,
        0,
        bytes(32),
    ):
        raise ValueError("manifest fields")
    if not length or length % 4 or dest + length > 0x4FFAC000:
        raise ValueError("payload range")
    payload_end = at + 64 + length
    data = image[at + 64 : payload_end]
    if len(data) != length or zlib.crc32(data) != crc:
        raise ValueError("payload CRC/length")
    if require_assets and len(image) == payload_end:
        raise ValueError("missing display assets")
    if len(image) != payload_end:
        asset_at = FLASH_ADDRESS - FLASH_BASE
        if payload_end > asset_at or len(image) > FLASH_SIZE - FLASH_BASE:
            raise ValueError("Flash asset overlap/range")
        if image[payload_end:asset_at] != b"\xff" * (asset_at - payload_end):
            raise ValueError("asset padding")
        verify_bundle(image[asset_at:])
    spm = segments[0][1]
    if spm.count(b"P4BIND01") != 1:
        raise ValueError("binding marker")
    bound, inverse = struct.unpack_from("<II", spm, spm.index(b"P4BIND01") + 8)
    if bound != crc or inverse != crc ^ 0xFFFFFFFF:
        raise ValueError("binding CRC")
    return size, length


if __name__ == "__main__":
    result = verify(Path(sys.argv[1]).read_bytes(), require_assets="--assets" in sys.argv[2:])
    print("Final image verified: SPM=%d B, payload=%d B" % result)
