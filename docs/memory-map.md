# Memory map (cache bypass, L2=0)

All ranges are half open. No allocation of PSRAM or lower free SRAM.

| Range | Role |
|---|---|
| 0x30100000..0x30102000 | SPM entry, max 8 KiB |
| 0x4ff00000..0x4ff18000 | 96 KiB display reservation, 92992 bytes of DMA descriptors/data |
| 0x4ff18000..0x4ff26000 | unused lower SRAM |
| 0x4ff26000..0x4ff28000 | 8 KiB boot stack; early uncached alias +0x40000000 |
| 0x4ff28000..0x4ff40000 | ROM reservation, untouched |
| 0x4ff40000..0x4ffac000 | .upper text/rodata/data then NOLOAD BSS, 432 KiB |
| 0x4ffac000..0x4ffc0000 | 80 KiB runtime stack |

Linker asserts entry and payload bounds, BSS vs stack, and ROM reservation.
The packer independently checks segment ranges and Flash capacity (16 MiB).
check_elf.py checks linked symbols, stack alignment and trap alignment.
No guard page/PMP protection exists yet. Link bounds do not detect stack overflow.

Flash offsets: 0x2000 ROM image, 0x40000 manifest, 0x40040 payload.
Build prints actual payload size and CRC; the large FF gap explains firmware file size.
