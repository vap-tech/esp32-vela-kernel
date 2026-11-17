#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --locked "$@"
espflash save-image --chip esp32p4 --min-chip-rev 1.3 --ignore-app-descriptor  --flash-mode dio --flash-size 16mb --flash-freq 80mhz  target/riscv32imafc-unknown-none-elf/release/vela-boot-esp32p4 target/ram-image.bin
python3 tools/image.py target/ram-image.bin target/firmware.bin
python3 tools/verify_image.py target/firmware.bin
