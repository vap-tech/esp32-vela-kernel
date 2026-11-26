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

## Kernel stack checks and measurement

`scripts/build.sh` builds the kernel with `cargo rustc --emit=link,asm=target/kernel.s`.
`tools/check_stack_sizes.py` reads CFI frame offsets from that same final LTO assembly.
Every recorded function must fit the 24 KiB single-frame budget. Missing or incomplete
CFI fails the build. The former 43,760-byte `rust_main` frame would fail this check.
The 24 KiB budget leaves room within each 40 KiB reservation for callees and the
256-byte marker, but does not prove that their combined usage fits. Handwritten
assembly, recursive call depth and dynamic stack adjustments are outside this check.
The ordinary early boot and emergency stacks are separate from this measurement.

Before entering Rust, boot fills both HP stack reservations with `0x56454c41`.
`cat /proc/kernel_stacks` reports one row per hart, in bytes:

- `usable` is 40,704 bytes, excluding the 256-byte boundary marker.
- `written_peak` is the distance from the top to the lowest changed word.
- `untouched` is the contiguous marker-filled portion above the boundary.
- `guard_ok` is 1 when all 64 boundary words retain the marker.

The paint is never refreshed until reboot, so startup and prior workload writes
remain visible. Read before and after representative workloads, including ELF
loading, filesystem reads, pipelines and display activity. No periodic stack scan
is installed. Each read scans at most 80 KiB, with other harts potentially active.
Reads use assembly loads without forming Rust references to live stack memory.

This is a sampled write high-water estimate, not an exact maximum SP measurement.
Untouched reserved frame space and writes equal to the marker can be missed.
Concurrent HP activity may change the result while reading. A healthy marker and
large untouched count do not prove absence of overflow on every execution path.
Likewise, software trap checks detect damage after it may have occurred. They do
not prevent an M-mode write into adjacent memory. Hardware prevention remains an
optional architecture project in the roadmap.

### Tab5 measurement, 2026-11-26

Release build, both HP harts online, framebuffer console active. After boot,
`ls /sd/bin`, `cat /sd/bin/wc.elf | wc`, `cat /proc/tasks | wc` and
`cat /proc/sdcard`, the final snapshot was:

| Hart | Usable bytes | Written peak | Untouched bytes | Guard intact |
|---|---:|---:|---:|---:|
| HP0 | 40,704 | 20,056 | 20,648 | 1 |
| HP1 | 40,704 | 2,332 | 38,372 | 1 |

HP1 increased from 1,888 bytes in the first snapshot to 2,332 after the commands.
`/proc/last_fault` reported `none`, SD errors and written sectors were zero,
and display startup reported 40 frames without underrun. This short read-only
workload is a baseline. It does not cover every filesystem, driver recovery or
exception path, and the scan itself contributes to stack use.

## Framebuffer console placement

Keep the bundled `fbcon` image (about 24 KiB of loadable segment address space,
rounded to a 32 KiB allocation) and its 8 KiB stack reservation (4 KiB usable,
4 KiB PMP-denied guard) in SRAM. The text grid and font are part of that image.
The 2 MiB scanout framebuffer remains in PSRAM. Keeping only the stack in SRAM
did not improve the observed redraw time while the renderer image stayed in PSRAM.

The four mostly blocked `init` supervisor threads each use a 64 KiB PSRAM
allocation with a 32 KiB usable stack and a 32 KiB guard. This trades 256 KiB
of external memory for 32 KiB of freed internal SRAM, allowing `fbcon` and the
keyboard service to run together. Guarded THREAD_CREATE and process isolation
remain enabled. Firmware boot requires enough SRAM for this service policy.

The first-render log uses system ticks (10 ms each), including scheduling delays.
Per-hart cycle subtraction is unsuitable when the renderer migrates between HP
harts. Compare similar cell counts and workloads, and include all services in the
check: a fast render alone does not establish sufficient memory for the keyboard
and supervisors.

On Tab5, 2026-11-26, the same first render of 618 cells at 180 MHz took
254 ticks (2.54 s) with the image in PSRAM, including when only its stack was
moved to SRAM. With the image and stack in SRAM, and supervisor stacks moved
to PSRAM, it took 8 ticks (80 ms), about 32 times faster. Both builds retained
the kernel trap checks. The pre-guard `e45a0bf` build also took 254 ticks with
its PSRAM image, so the regression predates the kernel stack checks.
The final boot had all four supervisors, keyboard, shell and display running,
`last_fault=none`, and intact HP boundary markers. These figures measure startup
redraw, not a separate worst-case full-screen scrolling benchmark.
