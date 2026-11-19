"""Regenerate the 64x64 alpha coverage asset with the system librsvg renderer."""
from pathlib import Path

import cairo
import gi

gi.require_version("Rsvg", "2.0")
from gi.repository import Rsvg

root = Path(__file__).resolve().parents[1]
panel = Rsvg.Handle.new_from_file(str(root / "orbit.svg"))
surface = cairo.ImageSurface(cairo.FORMAT_A8, 64, 64)
viewport = Rsvg.Rectangle()
viewport.width = viewport.height = 64
panel.render_document(cairo.Context(surface), viewport)
surface.flush()
assert surface.get_stride() == 64
(root / "src/platform/src/board/orbit.a8").write_bytes(bytes(surface.get_data()))
