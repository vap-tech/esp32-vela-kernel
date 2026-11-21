"""Convert PNG masters into Flash RLE assets and small Rust metadata."""

from pathlib import Path
from itertools import groupby
import struct
import sys

from PIL import Image
from flash_assets import bundle, NAMES


def encode(image):
    if image.size != (1280, 720):
        raise ValueError("Expected a 1280x720 landscape image")
    # Counterclockwise storage puts the keyboard connector along the bottom.
    portrait = image.convert("RGB").transpose(Image.Transpose.ROTATE_90)
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
    output.mkdir(parents=True, exist_ok=True)
    streams = []
    for name in NAMES:
        with Image.open(root / (name + ".png")) as image:
            streams.append(encode(image))
    data = bundle(streams)
    metadata = ["pub const HEADER: [u8; 64] = " + repr(list(data[:64])) + ";"]
    for i, name in enumerate(["BOOT", "DIAGNOSTIC"]):
        address, length, crc = struct.unpack_from("<III", data, 16 + i * 12)
        metadata.append(
            f"pub const {name}: Asset = Asset {{ address: {address}, length: {length}, crc: {crc} }};"
        )
    (output / "display_assets.rs").write_text("\n".join(metadata) + "\n")
    if "--metadata-only" not in sys.argv[2:]:
        (output / "display-assets.bin").write_bytes(data)
