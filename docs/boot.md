# Boot contract: ESP32-P4 rev1.3 only

1. ROM loads the compact image at flash 0x2000 into SPM 0x30100000.
2. Entry masks interrupts, asserts HP1 reset and gates HP1 clock, enables the
   inherited SRAM access path (0x500e5060 bit 0), installs an SPM fatal vector.
3. Uses boot stack through uncached alias 0x8ff28000; disables RTC/MWDT watchdogs
   including flashboot bits and super-watchdog. Timer watchdog writes set UPDATE_EN.
4. Stops cache autoload, double-writes back L1 data banks with bounded polling,
   sets L1 bypass=0x13, L2 bypass=0x20, L2 size=0, L2 line=8. Fences precede SRAM use.
5. Switches boot SP to 0x4ff28000. ROM esp_rom_spiflash_read at 0x4fc00158 reads
   the 64-byte manifest from 0x40000, then payload from 0x40040 into upper SRAM.
6. Validates magic P4UPPER1, version 1, linked destination and exact linked size,
   alignment, stack bound, zero reserved fields, source and bound CRC/inverse in SPM.
   Computes reflected CRC32 (EDB88320), with standard initial/final XOR.
7. Fences, zeros BSS, switches SP to 0x4ffc0000, installs runtime mtvec and enters Rust.
8. Rust reads back cache mode, emits banner and accepts diagnostic commands.

The startup must not call unloaded Rust, use upper constants or initialized data
before load. Only SPM and the ROM flash reader execute until payload verification.
There are no relocations at runtime. Linker fixes VMA; all initialized data is
packed into .upper. No DMA sections, heap or external RAM are initialized.
Early failures emit one bounded diagnostic letter: T trap, W cache timeout,
H header/binding/layout, R ROM read failure, C payload CRC. Then permanent halt.
A host attaching late may miss this letter. Runtime panic/trap output is also bounded.
The ROM flash call itself has no software timeout; this is a retained ROM dependency.

Image tools parse espflash RAM image, verify XOR checksum, SHA256 and canonical
layout, require one SPM and one upper segment in exact supported ranges, patch
the CRC binding, regenerate ROM checksum/SHA and append FF padding, manifest, payload.
Only SPM remains a ROM load segment. Manifest is little-endian 8s + six u32 + 32 zero
bytes: magic, version, destination, length, CRC32, source, flags, reserved.
CRC binding catches mixed bootstrap/payload updates; it provides no authentication
or power-failure recovery. The gap is reserved for this image and is flashed too.

Runtime fatal vector is direct M-mode mtvec (64-byte aligned), with mie and MIE off.
Trap frame saves all 30 non-SP/nonzero integer registers, original SP, mcause,
mepc, mtval, mstatus. P4 CLIC mcause includes mode fields; observed breakpoint
raw cause is 0x30000003 (exception code 3). No context restoration is implemented.

See provenance for why the hardware sequence was retained. Cold power-on remains
an explicit validation task; no claim of universal P4 or other-SoC compatibility.
