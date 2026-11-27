#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo fmt --manifest-path examples/upper/Cargo.toml -- --check
cargo check --workspace --locked
host=$(rustc -vV | sed -n 's/^host: //p')
cargo test --locked -p vela-abi -p vela-kernel -p vela-lp -p vela-user -p vela-sdk --lib --target "$host"
cargo test --locked -p vela-user --bin vela-wc --target "$host"
./scripts/check_usb_bot.sh
./scripts/check_usermem.sh
python3 -m unittest discover -s tools -p 'test_*.py'
./scripts/build-tools.sh
./scripts/build-app.sh
./scripts/build-app.sh examples/upper/Cargo.toml files
./scripts/build.sh
python3 tools/check_elf.py target/riscv32imafc-unknown-none-elf/release/vela-boot-esp32p4
