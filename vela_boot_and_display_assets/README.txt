VELA / Boot screen + LCD test screen
===================================

Target: M5Stack Tab5, ESP32-P4, landscape 1280x720.

Files:
  vela_boot.svg                   SVG master for boot screen
  vela_boot.png                   rasterized boot screen, 1280x720
  vela_display_test.svg           SVG master for pixel/display test
  vela_display_test.png           rasterized display test, 1280x720

The original orbit symbol is retained, scaled and modestly refined.
The orange pixel crab is original pixel art inspired by the Rust ecosystem.

Generate the display RGB565 buffer from the PNG during asset conversion.
Apply the rotation required by the panel's scanout orientation.

Boot SVG has a subtle background gradient; the test SVG uses flat colors.
The SVG uses text elements and thus needs installed font(s) to render exactly;
the provided PNG files preserve the rasterized fonts.

Vela integration
----------------
The build runs tools/display_assets.py (Python 3 with Pillow).
It generates counterclockwise-rotated RGB565 run streams in target/display-assets.
The bundle is appended at Flash offset 0x100000 by tools/flash_assets.py.
Cargo OUT_DIR contains only the small metadata included by the firmware.
The renderer reads Flash in 4 KiB chunks and expands into PSRAM.
The keyboard connector is along the bottom edge in this landscape orientation.
The command display diag selects the diagnostic image, and display boot
selects the boot artwork. Both use one kernel-owned PSRAM framebuffer.
