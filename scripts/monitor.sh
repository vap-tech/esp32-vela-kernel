#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
exec espflash monitor --port "${1:-/dev/ttyACM0}" --non-interactive
