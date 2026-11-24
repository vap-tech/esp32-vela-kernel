# Boot contract: ESP32-P4 rev1.3

1. ROM loads the compact `.entry` image at flash `0x2000` into SPM `0x30100000`.
2. `_start` masks interrupts, asserts HP1 global reset (`0x500e60c0` bit 8) and gates HP1 clock (`0x500e6014` bit 4) before touching caches or SRAM, enables the inherited SRAM access path (`0x500e5060` bit 0), and installs an early SPM fatal vector (`_boot_trap`).
3. Switches to the boot stack via the uncached alias `0x8ff28000` and disables RTC/MWDT watchdogs (including flashboot bits and super-watchdog; timer watchdog writes set `UPDATE_EN`).
4. Stops cache autoload, double-writes back L1 data banks with bounded polling, temporarily sets `L1 bypass = 0x13`, `L2 bypass = 0x20`, `L2 size = 0`, `L2 line = 8` (reclaiming L2 as SRAM), and issues fences.
5. Switches boot SP to `0x4ff28000`. ROM `esp_rom_spiflash_read` (`0x4fc00158`) reads the 64-byte manifest from `0x40000`, then the payload from `0x40040` into upper SRAM (`0x4ff40000`).
6. Validates magic `P4UPPER1`, version 1, linked destination and exact linked size, alignment, stack bound, zero reserved fields, source, and bound CRC/inverse in SPM. Computes reflected CRC32 (`0xedb88320`) with standard initial/final XOR.
7. Zeroes `.bss`, enables HP0's 16 KiB `L1-ICache0` (`L1 bypass = 0x12`, keeping `L1-DCache` bypassed) and invalidates `0x4ff00000..0x4ffc0000` in 4 KiB windows, switches SP to `_stack_top` (`0x4ffc0000`), installs `vela_trap_entry` in `mtvec`, and calls `rust_main`.
8. `rust_main` initializes counters, board I²C, console, `Runtime` (scheduler, PMP, IPC, events, DMA, PSRAM/SRAM `RegionAllocator`), loads the bundled U-mode static PIE services (`sysd`, `init`, `console`, `kbd`, `fbcon`, `ext4fs`), starts SYSTIMER, CLIC IRQ routing, and LP Sentinel, then brings up HP1 (`soc::smp::start_hp1` with `L1-ICache1` enabled and its 40 KiB stack at `_hp1_stack_top`), kicks HP1 via IPI, and enters U-mode on HP0.

## Early-boot invariants

- Stage 1 must not call unloaded Rust or reference upper constants/data before payload CRC verification. Only SPM and the ROM flash reader execute until verification passes.
- There are no kernel relocations at runtime; linker fixes VMA and packs initialized data into `.upper`.
- Early failures emit one bounded diagnostic letter on USB Serial/JTAG: `T` trap, `W` cache timeout, `H` header/binding/layout, `R` ROM read failure, `C` payload CRC, then halt permanently.
- The ROM flash call itself has no software timeout; this is a retained ROM dependency.

## Image format and runtime traps

Image tools (`tools/image.py`) parse the `espflash` RAM image, verify XOR checksum, SHA256, and canonical layout, require one SPM and one upper segment in exact supported ranges, patch the CRC binding, regenerate ROM checksum/SHA, and append `0xFF` padding, manifest, and payload. Only SPM remains a ROM load segment. The manifest is little-endian `8s` + six `u32` + 32 zero bytes: `magic`, `version`, `destination`, `length`, `crc32`, `source`, `flags`, `reserved`.

Runtime traps enter `vela_trap_entry` in SPM (64-byte aligned, CLIC vectored/common entry), saving all integer registers, `mepc`, `mcause`, `mtval`, and `mstatus`, and saving/restoring per-thread RV32F FPU + ESP32-P4 PIE (`Xespv2p1`) state on context switches. Unhandled M-mode faults print a bounded trap dump and halt; U-mode faults terminate only the offending process and release its resources.

See [provenance.md](provenance.md) for the retained hardware sequences.
