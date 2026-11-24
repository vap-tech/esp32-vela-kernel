#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
target=riscv32imac-unknown-none-elf
cargo_dir="$PWD/target/user-tools"
linker="$PWD/src/user/link.x"
export CARGO_ENCODED_RUSTFLAGS=$'-Crelocation-model=pie\x1f-Cstrip=symbols\x1f-Clink-arg=-pie\x1f-Clink-arg=-zmax-page-size=64\x1f-Clink-arg=-T'"$linker"
cargo build --locked --release -p vela-user --target "$target" --target-dir "$cargo_dir" --bin vela-uptime --bin vela-cat
mkdir -p target/sd/bin
for name in uptime cat; do
    image="$cargo_dir/$target/release/vela-$name"
    if (( $(stat -c %s "$image") > 8192 )); then
        echo "Tool $name exceeds the 8 KiB SPAWN_IMAGE limit" >&2
        exit 1
    fi
    install -m 0644 "$image" "target/sd/bin/$name.elf"
done
