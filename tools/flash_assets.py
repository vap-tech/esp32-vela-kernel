"""Pack and validate display RLE data in a fixed Flash asset area."""

from pathlib import Path
import struct
import sys
import zlib

FLASH_ADDRESS = 0x100000
FLASH_SIZE = 16 * 1024 * 1024
HEADER_SIZE = 64
NAMES = ("vela_boot", "vela_display_test")
MAGIC = b"VELAIMG1"
PIXELS = 720 * 1280


def validate_stream(data):
    if not data or len(data) % 4:
        raise ValueError("invalid RGB565 run stream length")
    total = 0
    for count, _ in struct.iter_unpack("<HH", data):
        if count == 0:
            raise ValueError("zero RGB565 run")
        total += count
    if total != PIXELS:
        raise ValueError("RGB565 pixel count mismatch")


def bundle(streams):
    if len(streams) != len(NAMES):
        raise ValueError("expected boot and diagnostic streams")
    header = bytearray(struct.pack("<8sII", MAGIC, 1, len(NAMES)))
    address = FLASH_ADDRESS + HEADER_SIZE
    for data in streams:
        validate_stream(data)
        header += struct.pack("<III", address, len(data), zlib.crc32(data))
        address += len(data)
    if address > FLASH_SIZE:
        raise ValueError("assets exceed Flash")
    header += bytes(HEADER_SIZE - len(header))
    return bytes(header) + b"".join(streams)


def verify_bundle(data):
    if len(data) < HEADER_SIZE:
        raise ValueError("truncated asset header")
    if struct.unpack_from("<8sII", data) != (MAGIC, 1, len(NAMES)):
        raise ValueError("asset header fields")
    if data[40:64] != bytes(24):
        raise ValueError("asset header reserved fields")
    position = HEADER_SIZE
    for i in range(len(NAMES)):
        address, length, crc = struct.unpack_from("<III", data, 16 + i * 12)
        if address != FLASH_ADDRESS + position or address + length > FLASH_SIZE:
            raise ValueError("asset address range")
        stream = data[position : position + length]
        if len(stream) != length or zlib.crc32(stream) != crc:
            raise ValueError("asset CRC/length")
        validate_stream(stream)
        position += length
    if position != len(data):
        raise ValueError("unexpected trailing asset bytes")


def attach(firmware, assets, flash_base):
    verify_bundle(assets)
    padding = FLASH_ADDRESS - flash_base - len(firmware)
    if padding < 0:
        raise ValueError("firmware overlaps asset area")
    return firmware + b"\xff" * padding + assets


if __name__ == "__main__":
    firmware, assets = map(Path, sys.argv[1:])
    from image import FLASH_BASE

    data = assets.read_bytes()
    firmware.write_bytes(attach(firmware.read_bytes(), data, FLASH_BASE))
    print(f"Display assets: Flash={FLASH_ADDRESS:#x}, bytes={len(data)}, CRCs validated")
