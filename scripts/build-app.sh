#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
manifest="${1:-examples/upper/Cargo.toml}"
app_bin="${2:-vela-upper}"
if [[ ! "$app_bin" =~ ^[a-zA-Z0-9_-]+$ ]]; then
    echo 'Expected a Cargo binary name' >&2
    exit 1
fi
app_target=riscv32imac-unknown-none-elf
app_build_dir="$PWD/target/apps-build"
app_linker="$PWD/src/user/link.x"
export CARGO_ENCODED_RUSTFLAGS=$'-Crelocation-model=pie\x1f-Cstrip=symbols\x1f-Clink-arg=-pie\x1f-Clink-arg=-zmax-page-size=64\x1f-Clink-arg=-T'"$app_linker"
cargo build --manifest-path "$manifest" --bin "$app_bin" --locked --release \
    --target "$app_target" --target-dir "$app_build_dir"
app_image="$app_build_dir/$app_target/release/$app_bin"
if (( $(stat -c %s "$app_image") > 1048576 )); then
    echo 'Application exceeds the 1 MiB runtime ELF limit' >&2
    exit 1
fi
mkdir -p target/apps
install -m 0644 "$app_image" "target/apps/$app_bin.elf"
echo "Application ready: target/apps/$app_bin.elf"
