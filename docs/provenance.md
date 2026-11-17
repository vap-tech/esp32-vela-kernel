# Hardware provenance and license

Reference read-only project: /home/kot/Projects/esp32-p4, Git HEAD
b28085103d7a68bce7888c9a9860ae1590c4272c, inspected 2026-11-17.
Local files may differ from HEAD; this identifies the repository, not a pristine snapshot.
MIT copyright 2026 vitalii petrenko is retained in adapted files; full notice:
[reference-LICENSE](reference-LICENSE).

Adaptations: entry.S takes only boot/cache/watchdog/ROM-load and CRC logic from
src/boot.S. Removes old IRQ dispatch/return, DMA clearing, rust_memory_boot and runtime.
Early failure output is bounded. ROM binding format retained for paired-image integrity.
console.rs is adapted from src/hal/usb_serial.rs; now uses minimal PAC and reports
format errors on polling exhaustion. tools/image.py retains the small reference
parser/encoder/packer because all of that format is used here; adds chip/origin checks.
No assets, web packaging, HAL wholesale, executor, wakers or HP worker were imported.
link.x uses the reference's measured SRAM reservations and stack ranges, simplified.

Read hardware references: boot.S, usb_serial/watchdog/clock/time/interrupt/memory/
multicore HAL modules, link.x, docs/boot.md, docs/stage2-boot.md, docs/memory.md,
image.sh, pack-image.py, flash.sh, rust-toolchain.toml.
Reference notes identify ESP-IDF v6.1 ROM ld/header as the source for ROM read address.
That SDK was not independently audited here. The actual ROM read/load sequence is
validated by Vela's flash boot and payload execution on the selected rev1.3 device.
Old project's Guition board identity is not inherited. Tab5 identity is configured,
not hardware-detected. Silicon identity/security/Flash were checked with espflash.

Multitasking addition: soc/esp32p4-v13/timer.rs uses only hardware setup sequences
from reference hal/time.rs and hal/interrupt.rs (MIT notice retained). No reference
scheduler, executor or concurrency code is imported. P4 CLIC details were checked
against local ESP-IDF components/riscv/vectors.S, vectors_clic.S, csr_clic.h and
esp_hw_support/port/esp32p4/cpu_region_protect.c. The new trap and PMP code are Vela
implementations; no SDK source text is copied. Rev1.x uses the legacy threshold
MMIO register and 128-byte PMP granularity. Shared user RX/private stack RW entries
are followed by explicit deny-all at PMP entry15; no locked entries are installed.
