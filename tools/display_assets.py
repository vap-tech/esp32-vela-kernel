"""Convert PNG masters into rotated RGB565 run streams in Cargo's OUT_DIR."""
from pathlib import Path
from itertools import groupby
import struct
import sys

from PIL import Image

def encode(image):
    if image.size != (1280, 720):
        raise ValueError("Expected a 1280x720 landscape image")
    portrait = image.convert("RGB").transpose(Image.Transpose.ROTATE_270)
    pixels = [(r >> 3) << 11 | (g >> 2) << 5 | (b >> 3) for r, g, b in portrait.getdata()]
    encoded = bytearray()
    for pixel, group in groupby(pixels):
        count = sum(1 for _ in group)
        while count:
            part = min(count, 65535)
            encoded += struct.pack("<HH", part, pixel)
            count -= part
    # Round-trip the compiled stream before handing it to the firmware.
    decoded = []
    for count, pixel in struct.iter_unpack("<HH", encoded):
        decoded.extend([pixel] * count)
    if decoded != pixels:
        raise ValueError("RGB565 run stream round-trip failed")
    return encoded

if __name__ == "__main__":
    root = Path(__file__).resolve().parents[1] / "vela_boot_and_display_assets"
    output = Path(sys.argv[1])
    for name in ["vela_boot", "vela_display_test"]:
        with Image.open(root / (name + ".png")) as image:
            (output / (name + ".rle")).write_bytes(encode(image))
