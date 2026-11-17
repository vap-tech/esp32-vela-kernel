#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
port="${1:-/dev/ttyACM0}"
if [[ "$port" != /dev/ttyACM0 ]]; then
 echo 'Only the selected Tab5 port /dev/ttyACM0 is authorized' >&2; exit 1
fi
./scripts/build.sh
info=$(espflash board-info --port "$port" 2>&1)
echo "$info"
for expected in 'esp32p4 (revision v1.3)' '16MB' 'e8:f6:0a:e2:fd:50' 'Secure Boot: Disabled' 'Flash Encryption: Disabled'; do
 if ! [[ "$info" == *"$expected"* ]]; then
  echo "Device mismatch: $expected" >&2; exit 1
 fi
done
espflash write-bin --port "$port" 0x2000 target/firmware.bin
