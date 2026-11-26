#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
python3 tools/display_assets.py target/display-assets
cargo rustc -p vela-boot-esp32p4 --release --locked "$@" -- --emit=link,asm=target/kernel.s
python3 tools/check_stack_sizes.py target/kernel.s
espflash save-image --chip esp32p4 --min-chip-rev 1.3 --ignore-app-descriptor  --flash-mode dio --flash-size 16mb --flash-freq 80mhz  target/riscv32imafc-unknown-none-elf/release/vela-boot-esp32p4 target/ram-image.bin
python3 tools/image.py target/ram-image.bin target/firmware.bin
python3 tools/flash_assets.py target/firmware.bin target/display-assets/display-assets.bin
python3 tools/verify_image.py target/firmware.bin --assets
