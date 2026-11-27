# Vela Architecture

Vela is a `no_std` bare-metal Rust microkernel targeting ESP32-P4 v1.3 on M5Stack Tab5 (dual-core `riscv32imafc` + `Xespv2p1` PIE HP0/HP1 SMP + `riscv32imac` LP Sentinel). All kernel and userspace state uses bounded static tables with no global heap or async runtime.

## Workspace Layout (`src/`)

The Cargo workspace contains six crates with a strict acyclic dependency graph and zero cross-crate `#[path]` inclusions:

- `src/abi` (`vela-abi`) — Syscall numbers/constants, unified File IPC protocol (`fs`), and HP↔LP Sentinel wire protocol v2 definitions. Depends only on `core`.
- `src/kernel` (`vela-kernel`) — Architecture- and SoC-independent microkernel core: typed IDs (`Pid`, `Tid`, `ProcSlot`, `ThreadSlot`, `Handle`, `Ticks`), `Errno`, `capability`, `event` (`EventTable`), `ipc`, `call` (`CallTable`), `memory` (`MemoryClass`, `RegionPool`, `RegionAllocator`), `process` (`AddressSpace`, `Processes`), `sched` (`Scheduler`), `elf` (`ET_EXEC` and static PIE validator/relocator), `dma` (`DmaController`, `DmaDirection`, `DmaTable`, `Lease`), `sentinel` (`RequestLink`, `WakeTicket`), `stats`, and cross-subsystem `lifecycle` orchestration. Pure `no_std` Rust; unit-tested on the host (`cargo test -p vela-kernel`).
- `src/lp` (`vela-lp`) — LP Sentinel core logic (`watch`, `journal`, `wakeup`, host-tested via `cargo test -p vela-lp`), shared MMIO helpers (`hw::{lp_counter, mailbox}`), and the standalone `vela-sentinel` RV32IMAC binary linked at LP SRAM `0x50108000`.
- `src/user` (`vela-user`) — Standalone U-mode runtime library (`sys`, `Buffer`, bump `Arena`, `mount`, `boot_cfg`, userspace `ina226` power driver, `kbd`, `ext4`, `fbcon`, and `sdcard` helpers, host-tested via `cargo test -p vela-user`), bundled flash static PIE services (`vela-sysd`, `vela-init`, `vela-console`, `vela-fbcon`, `vela-kbd`, `vela-ext4fs`), and standalone `/sd/bin/` utilities (`vela-shell`, `vela-cat`, `vela-ls`, `vela-uptime`, `vela-sdexport`, `vela-wc`, `vela-ramdev`, `vela-readbench`).
- `src/sdk` (`vela-sdk`) — Application-facing file cursors, stdin/stdout, owned PSRAM buffers and explicit-authority process launching. Depends on `vela-abi` and `vela-user`; independent examples live in `examples/`.
- `src/platform` (`vela-boot-esp32p4`) — Two-stage SPM/SRAM bootstrap (`entry.S`), minimal ESP32-P4 PAC, SPM-resident CLIC trap entry/exit (`arch/trap.S`), cached 16-entry NAPOT PMP compiler (`arch/pmp.rs`), SoC drivers (USB Serial/JTAG console, Full-Speed USB OTG MSC export, SYSTIMER, HP0/HP1 SMP + IPI, GDMA-AHB, DW-GDMA/DSI/ST7121/PSRAM display, 4-bit SDMMC IDMAC host + SPI fallback over `LDO_VO4` + `GPIO39..44`, TRNG/ChaCha20 CSPRNG, GPIO level IRQ, system & Ext.Port1 I²C, LP loader), boot policy (`boot.rs`), and thin syscall dispatchers (`dispatch/`).

Dependency direction:
- `vela-boot-esp32p4 -> {vela-kernel, vela-lp, vela-abi}`
- `vela-user -> vela-abi`
- `vela-lp -> vela-abi`
- `vela-kernel` has zero internal crate dependencies.

## Memory Protection, Cache Split & Standalone ELF Services

- Two-stage boot loads `.entry` (`entry.S` plus `arch/trap.S`: `vela_trap_entry`, `vela_resume`, `vela_idle_entry`, `vela_hp1_entry`) into 8 KiB zero-wait-state SPM (`0x30100000`), disables watchdogs, holds HP1 in reset during SRAM load, reclaims L2 cache as internal SRAM, verifies CRC32 of the upper SRAM payload (`0x4ff40000`), zeroes `.bss`, enables and invalidates HP0's 16 KiB `L1-ICache0` while keeping `L1-DCache` bypassed (`0x12`), enters `rust_main`, and later brings HP1 online with `L1-ICache1` enabled.
- Keeping `L1-DCache` bypassed preserves direct zero-flush data coherency across SRAM, PSRAM, `GDMA-AHB`, `DW-GDMA`, and `SDMMC IDMAC`, while `Runtime::load_elf_in` explicitly invalidates `L1-ICache0`/`L1-ICache1` + `fence.i` across newly written executable segments before user execution.
- `RegionAllocator` manages two disjoint NAPOT pools: internal SRAM (4 KiB page granularity) and the entire 32 MiB external AP HEX PSRAM (`0x88000000..0x8a000000`, 512 × 64 KiB MMU pages). When display scanout is active (`vela-fbcon`), the display driver dynamically allocates a 2 MiB region from `RegionAllocator`, maps its 32 MMU pages, and marks the framebuffer `InFlight` on `DW-GDMA CH0`; when display output is stopped (`echo off > /sys/display`), the 2 MiB framebuffer is unmapped and returned so applications can allocate up to the full 32 MiB of PSRAM.
- Userspace code is never linked into kernel `.text`/`.rodata`. `build.rs` compiles the bundled boot services (`vela-sysd`, `vela-init`, `vela-console`, `vela-fbcon`, `vela-kbd`, `vela-ext4fs`) as stripped RV32IMAC static PIE ELFs and loads them at boot through the same `vela_kernel::elf::Image` validator, `R_RISCV_RELATIVE` relocator, and `RegionAllocator` used for runtime `ET_EXEC`/`ET_DYN` programs launched from `/sd/bin/`.
- Every process owns an `AddressSpace` of up to 6 disjoint W^X NAPOT regions (`READ`, `READ | WRITE`, or `READ | EXECUTE`) compiled into a slim cached PMP image (`pmpaddr0..5` + `pmpcfg0..1` over permanent `entry15` deny-all) activated before `mret`, with CSR readback on the first activation of each new `(ProcSlot, revision)` and synchronous IPI PMP shootdown across harts on address-space updates.

## Capabilities & Object Lifecycle

- Every process holds a 16-slot generational capability table (`Handle = generation << 8 | slot`). Global table indices never cross the syscall ABI.
- Kernel objects (`Object`) cover five classes:
  - `Endpoint { index, generation }` — Bounded 4-message, 96-byte FIFO queues and synchronous `IPC_CALL` / `IPC_REPLY` / `IPC_REPLY_RECV` endpoints.
  - `Memory { owner, region }` — NAPOT SRAM or PSRAM regions supporting one-level `READ` / `READ | WRITE` shared mappings (`MEM_GRANT`) and per-channel `InFlight` DMA ownership tracking.
  - `Event { index, generation }` — Managed interrupt-backed event slots (`EventTable`, up to 8 hardware CLIC lines: USB Serial/JTAG RX and Tab5 Keyboard `GPIO50`) supporting `EVENT_WAIT(handle, timeout_ticks)`, `IRQ_ACK(handle)`, and dynamic `IRQ_BIND` / `IRQ_UNBIND` lifecycle.
  - `DmaChannel { index, generation }` — Managed DMA channel capabilities (`DmaTable`: `GDMA-AHB CH1` memory copy and `DW-GDMA CH0` DSI video scanout) enforcing controller-specific `MemoryClass` compatibility, 64-byte cache-line alignment, and `InFlight` region holds across active transfers.
  - `Authority(ProcessSpawn | Display | Sentinel | IrqControl | I2c | Storage)` — Explicit capability-gated control over process spawning (with up to 4 attenuated child handles via `PASS_ON_SPAWN`), display scanout, LP Sentinel supervision, IRQ slot binding, board-filtered I²C transfers (`I2C_TRANSFER`), and partition-bounded storage reads. No syscall grants authority by checking a hardcoded `Pid`.
- `CAP_GRANT` and `MEM_GRANT` enforce one-level delegation (`GRANT` and `CONTROL` are never delegable). `CAP_REVOKE`, `IRQ_UNBIND`, `MEM_FREE`, `ENDPOINT_CLOSE`, and final thread exit run through `vela_kernel::lifecycle`, unmapping revoked shared memory, waking affected `Wait::Receive` threads with `-EFAULT`, failing orphaned RPC callers or event waiters with `-EPIPE`, deferring reuse of `InFlight` DMA regions until transfer completion, and automatically masking abandoned or revoked hardware IRQ lines at both CLIC and peripheral `INT_ENA`/`INT_CLR` before releasing `RegionAllocator` (and PSRAM MMU pages), DMA, and Sentinel wake leases.

## Scheduling, Deadlines & Synchronous RPC

- `Scheduler<C>` runs at 100 Hz (SYSTIMER target 0) across both HP0 and HP1 with 4 priority levels (`0..=3`), per-thread hart affinity (`1..=3`), round-robin rotation within equal effective priority, and bounded `O(TASKS²)` transitive priority inheritance across `Wait::Reply` chains.
- `IPC_CALL` rejects self-RPC and walks `Wait::Reply` chains up to `TASKS` to reject transitive RPC cycles with `-EDEADLK` before blocking. When the endpoint queue is empty and a server thread is already blocked in `Wait::Receive`, `try_direct_ipc_call` hands the payload and reply token directly to the server without staging through the endpoint queue, while `IPC_REPLY_RECV` replies and re-enters `Wait::Receive` in a single trap.
- All time-bounded waits (`Wait::Sleep`, `Wait::Reply`, `Wait::Event`, and generic `Wait::Device { domain, token, pointer }`) store their wakeup limit in `Thread::deadline: Option<Ticks>`, expired in a single `O(TASKS)` pass inside `Scheduler::tick`.

## Safety & Concurrency Invariants

- Dual HP cores (`HP0` and `HP1`) synchronize kernel state with a single non-reentrant M-mode spinlock (`soc::smp::KERNEL_LOCK`) held only while `mstatus.MIE = 0`. Cross-hart wakeups and PMP invalidations use dedicated software IPI lines with synchronous shootdown acknowledgment.
- Machine interrupts remain disabled (`mstatus.MIE = 0`) throughout every Rust kernel path. Interrupts are enabled only in U-mode (`MPIE = 1`, `MPP = U`) or inside the dedicated assembly idle loop (`vela_idle_entry`, `MPP = M`), which holds no Rust references to `RUNTIME`.
- All user pointers are validated against the active `AddressSpace` permissions before `copy_from_user` / `copy_to_user_within` / `copy_between_users`. During `DMA_COPY`, `AddressSpace::dma_access` temporarily zeroes U-mode PMP permissions on the DMA page and `RegionAllocator` marks the buffer `InFlight` until the completion interrupt verifies transfer status.
- `Runtime` is the sole runtime owner of `Console` (except non-returning `fatal`/`panic` takeover) and `DisplayControl` (`psram::Controller` + `scanout::Scanout`); all raw SoC MMIO/ROM operations are `unsafe fn`, and BSP/SoC diagnostics are returned by value rather than instantiating second `Console` handles.
- HP↔LP Sentinel communication uses a single hardware mailbox request slot managed by `RequestLink`: at most one request owns `(ARG0, ARG1, REQUEST_SEQ, COMMAND)` until LP answers that exact `sequence`; deadline expiration transitions the link to `offline` (`-ETIMEDOUT`) with idempotent doorbell reassertion until recovery.
- Hardware cycle accounting (`CycleAccounting`) tracks per-thread and per-hart `user_cycles`, `kernel_cycles`, and `idle_cycles` via 64-bit `mcycle`, integrates wall-clock CPU capacity across `40 MHz` XTAL ↔ `180 MHz` CPLL transitions against the 16 MHz SYSTIMER, and measures comparator-to-ISR latency on runtime timer ticks.


## SD bulk-read boundary

`ext4fs` resolves file offsets and extents in U-mode. For aligned data,
`SD_PARTITION_READ_BULK` checks SD READ authority and a live server-bound
RPC token, then validates the complete partition range, writable bulk grant
and current client mappings. Each operation reads at most 4 KiB into private
kernel DMA SRAM and copies exactly the permitted bytes into the client.
KERNEL_LOCK protects the DMA buffer and mappings through completion.
Unaligned leading bytes and sparse/unwritten spans use the service staging
buffer. Large client requests are served by multiple bounded SD operations.

Detailed timings require the `BULK_READ_PROFILE` request flag on a recognized
bulk-read operation. ext4fs selects a stack-owned `Cell<bool>` for each
request; its Partition reads, including metadata, use the matching
`IO_PROFILE` syscall flag. Kernel SD and IPC_BULK_IO timing snapshots run
for flagged operations. Call counters count every operation.

Ordinary successful bulk reads return the five-byte status/count header.
Profiled ext4 replies include service timings, with sysd adding forwarding
time for a total of 29 bytes. Timings are nested and kernel timing counters
are cumulative. `readbench profile` selects the diagnostic path; its default
selects ordinary reads. Current commands and measurements are in the README
and roadmap.
