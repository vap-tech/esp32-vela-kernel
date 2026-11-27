#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
host=$(rustc -vV | sed -n 's/^host: //p')
temp=$(mktemp -d)
trap 'rm -rf -- "$temp"' EXIT
mkdir "$temp/src"
ln -s "$PWD/src/platform/src/usermem.rs" "$temp/src/usermem.rs"
cat > "$temp/Cargo.toml" <<MANIFEST
[package]
name = "vela-usermem-test"
version = "0.0.0"
edition = "2024"

[dependencies]
vela-kernel = { path = "$PWD/src/kernel" }
MANIFEST
cat > "$temp/src/lib.rs" <<'RUST'
pub mod usermem;
RUST
CARGO_TARGET_DIR="$PWD/target/host-usermem" cargo test --offline --manifest-path "$temp/Cargo.toml" --target "$host"
