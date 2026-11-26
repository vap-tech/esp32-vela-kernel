# Memory map (L1-ICache0/1 enabled, L1-DCache bypass, L2=0)

All ranges are half open and match `src/platform/link.x`. `RegionAllocator` manages upper internal SRAM and the full 32 MiB external PSRAM pool (`0x88000000..0x8a000000`); `vela-fbcon` allocates its 2 MiB NAPOT framebuffer from this pool when display scanout is active and returns it when display output is stopped (`echo off > /sys/display`).

| Range | Role |
|---|---|
| `0x30100000..0x30102000` | SPM entry + CLIC trap vectors (`.entry`), max 8 KiB |
| `0x4ff00000..0x4ff24000` | 144 KiB `.display` DMA reservation: display LLI/band buffers + SDMMC IDMAC descriptor chain (4 KiB) + 32 KiB SDMMC DMA ring |
| `0x4ff24000..0x4ff26000` | Two independent 4 KiB emergency kernel stacks (HP0 top 0x4ff25000, HP1 top 0x4ff26000) |
| `0x4ff26000..0x4ff28000` | 8 KiB boot stack; early uncached alias `+0x40000000` (`0x8ff28000`) |
| `0x4ff28000..0x4ff40000` | 96 KiB ROM reservation, untouched |
| `0x4ff40000..0x4ffaa000` | `.upper` text/rodata/data + NOLOAD `.bss` + SRAM `RegionAllocator` pool (424 KiB) |
| `0x4ffaa000..0x4ffac000` | 8 KiB kernel GDMA-AHB arena (`_dma_page` 4 KiB + `_dma_descriptors` 4 KiB) |
| `0x4ffac000..0x4ffc0000` | 80 KiB kernel stack reservations, each with a 256-byte lower boundary marker. 40 KiB HP1 (`_hp1_stack_top = 0x4ffb6000`) + 40 KiB HP0 (`_stack_top = 0x4ffc0000`) |
| `0x48000000..0x4a000000` | External PSRAM bus window (32 MiB, MMU pages `0..512`), DW-GDMA AXI address |
| `0x88000000..0x8a000000` | Managed uncached CPU PSRAM pool (32 MiB, MMU pages `0..512`, 64 KiB granularity) |
| `0x50108000..0x50110000` | LP SRAM: standalone RV32IMAC LP Sentinel firmware |

Linker assertions check `.entry`, `.display`, `.upper`, `.bss`, `_dma_page`, stack bounds, and the ROM reservation. The packer independently checks segment ranges and Flash capacity (16 MiB). `tools/check_elf.py` verifies linked symbols, stack alignment, and trap alignment. M-mode trap entry validates SP before saving a kernel-origin trap frame. Entry and return scan each hart’s 256-byte boundary marker. A failed check switches to that hart’s emergency stack, prints its hart/SP/PC, and halts it. These software checks detect corruption at trap boundaries; an intervening M-mode write can already have damaged memory.

Boot initializes `Runtime` fields directly in static storage. This removes the oversized aggregate temporary from HP0’s stack. The current release `rust_main` frame is 17,088 bytes (previously 43,760 bytes, exceeding its 40 KiB stack reservation). These figures describe that function’s frame, not the maximum nested call depth.

Flash offsets are `0x2000` ROM image, `0x40000` manifest, `0x40040` SRAM payload, and `0x100000` display asset bundle (`VELAIMG1` header + two RLE images, retained for artwork/wallpapers). Build output reports actual `.entry`, payload, `.bss`, and CRC32 values.
