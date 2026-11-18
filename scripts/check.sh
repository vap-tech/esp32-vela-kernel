#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo check --workspace --locked
host=$(rustc -vV | sed -n 's/^host: //p')
cargo test --locked -p vela-kernel --target "$host"
mkdir -p target/lp-tests
for module in watch journal wakeup; do
    rustc --edition=2024 --test "lp/sentinel/$module.rs" -o "target/lp-tests/$module"
    "target/lp-tests/$module"
done
python3 -m unittest discover -s tools -p 'test_*.py'
./scripts/build.sh
python3 tools/check_elf.py target/riscv32imafc-unknown-none-elf/release/vela-boot-esp32p4
