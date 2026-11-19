#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo check --workspace --locked
host=$(rustc -vV | sed -n 's/^host: //p')
cargo test --locked -p vela-kernel -p vela-lp --lib --target "$host"
python3 -m unittest discover -s tools -p 'test_*.py'
./scripts/build.sh
python3 tools/check_elf.py target/riscv32imafc-unknown-none-elf/release/vela-boot-esp32p4
