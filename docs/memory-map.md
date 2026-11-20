# Memory map (L1-ICache0 enabled, L1-DCache bypass, L2=0)

All ranges are half open. The display driver owns the first 2 MiB PSRAM window;
the kernel `RegionAllocator` manages upper internal SRAM and the remaining 30 MiB PSRAM pool.

| Range | Role |
|---|---|
| 0x30100000..0x30102000 | SPM entry + trap vectors (`.entry`), max 8 KiB |
| 0x4ff00000..0x4ff18000 | 96 KiB display reservation, 93184 bytes of DMA descriptors/data |
| 0x4ff18000..0x4ff26000 | unused lower SRAM |
| 0x4ff26000..0x4ff28000 | 8 KiB boot stack; early uncached alias +0x40000000 |
| 0x4ff28000..0x4ff40000 | ROM reservation, untouched |
| 0x4ff40000..0x4ffac000 | .upper text/rodata/data then NOLOAD BSS + SRAM `RegionAllocator` pool, 432 KiB |
| 0x4ffac000..0x4ffc0000 | 80 KiB runtime stack (+ reserved 4 KiB AHB DMA page) |
| 0x48000000..0x48200000 | kernel display PSRAM window (2 MiB, MMU pages 0..32), bus address |
| 0x88000000..0x88200000 | uncached CPU alias of the 2 MiB display framebuffer window |
| 0x88200000..0x8a000000 | managed user PSRAM pool (30 MiB, MMU pages 32..512, 64 KiB granularity) |

Linker asserts entry and payload bounds, BSS vs stack, and ROM reservation.
The packer independently checks segment ranges and Flash capacity (16 MiB).
check_elf.py checks linked symbols, stack alignment and trap alignment.
No guard page/PMP protection exists for the kernel stack yet. Link bounds do not detect stack overflow.

Flash offsets are 0x2000 ROM image, 0x40000 manifest, 0x40040 SRAM payload,
and 0x100000 display asset header followed by two RLE images. The asset
area is read in chunks at runtime. It has no SRAM load segment.
Build prints actual payload size and CRC; the large FF gap explains firmware file size.
