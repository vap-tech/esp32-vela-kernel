#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
host=$(rustc -vV | sed -n 's/^host: //p')
temp=$(mktemp -d)
trap 'rm -rf -- "$temp"' EXIT
mkdir "$temp/src"
ln -s "$PWD/src/platform/src/soc/usb_storage/bot.rs" "$temp/src/bot.rs"
cat > "$temp/Cargo.toml" <<'MANIFEST'
[package]
name = "vela-usb-bot-test"
version = "0.0.0"
edition = "2024"

[dependencies]
usb-device = "=0.3.2"
MANIFEST
cat > "$temp/src/lib.rs" <<'RUST'
#[cfg(test)]
const SECTOR: usize = 512;
#[cfg(test)]
mod bot;
RUST
CARGO_TARGET_DIR="$PWD/target/host-usb-bot" cargo test --offline --manifest-path "$temp/Cargo.toml" --target "$host"
