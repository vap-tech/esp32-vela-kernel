# Vela Architecture

Vela is a `no_std` bare-metal Rust microkernel targeting ESP32-P4 v1.3 on M5Stack Tab5 (`riscv32imafc` HP hart0 + `riscv32imac` LP Sentinel). All kernel and userspace state uses bounded static tables with no global heap or async runtime.

## Workspace Layout (`src/`)

The Cargo workspace contains five crates with a strict acyclic dependency graph and zero cross-crate `#[path]` inclusions:

- `src/abi` (`vela-abi`) — Syscall numbers/constants and HP↔LP Sentinel wire protocol v2 definitions. Depends only on `core`.
- `src/kernel` (`vela-kernel`) — Architecture- and SoC-independent microkernel core: typed IDs (`Pid`, `Tid`, `ProcSlot`, `ThreadSlot`, `Handle`, `Ticks`), `Errno`, `capability`, `event` (`EventTable`), `ipc`, `call` (`CallTable`), `memory` (`MemoryClass`, `RegionPool`, `RegionAllocator`), `process` (`AddressSpace`, `Processes`), `sched` (`Scheduler`), `elf` (`ET_EXEC` and static PIE validator/relocator), `dma` (`DmaController`, `DmaDirection`, `DmaTable`, `Lease`), `sentinel` (`RequestLink`, `WakeTicket`), `stats`, and cross-subsystem `lifecycle` orchestration. Pure `no_std` Rust; unit-tested on the host (`cargo test -p vela-kernel`).
- `src/lp` (`vela-lp`) — LP Sentinel core logic (`watch`, `journal`, `wakeup`, host-tested via `cargo test -p vela-lp`), shared MMIO helpers (`hw::{lp_counter, mailbox}`), and the standalone `vela-sentinel` RV32IMAC binary linked at LP SRAM `0x50108000`.
- `src/user` (`vela-user`) — Standalone U-mode runtime library (`sys`, `Buffer`, bump `Arena`, userspace `ina226` power driver, host-tested via `cargo test -p vela-user`) and isolated static PIE service binaries (`vela-procfs`, `vela-shell`, `vela-console`), plus assembly test programs (`apps/{hello,pic,dma}`).
- `src/platform` (`vela-boot-esp32p4`) — Two-stage SPM/SRAM bootstrap (`entry.S`), minimal ESP32-P4 PAC, SPM-resident CLIC trap entry/exit (`arch/trap.S`), cached 16-entry NAPOT PMP compiler (`arch/pmp.rs`), SoC drivers (USB Serial/JTAG console, SYSTIMER, GDMA-AHB, DW-GDMA/DSI/ST7121/PSRAM display, open-drain system I²C, LP loader), boot policy (`boot.rs`), and thin syscall dispatchers (`dispatch/`).

Dependency direction:
- `vela-boot-esp32p4 -> {vela-kernel, vela-lp, vela-abi}`
- `vela-user -> vela-abi`
- `vela-lp -> vela-abi`
- `vela-kernel` has zero internal crate dependencies.

## Memory Protection, Cache Split & Standalone ELF Services

- Two-stage boot loads `.entry` (`entry.S` plus `arch/trap.S`: `vela_trap_entry`, `vela_resume`, `vela_idle_entry`) into 8 KiB zero-wait-state SPM (`0x30100000`), disables watchdogs, holds HP1 in reset, reclaims L2 cache as internal SRAM, verifies CRC32 of the upper SRAM payload (`0x4ff40000`), zeroes `.bss`, enables and invalidates HP0's 16 KiB `L1-ICache0` while keeping `L1-DCache` bypassed (`0x12`), and enters `rust_main`.
- Keeping `L1-DCache` bypassed preserves direct zero-flush data coherency across SRAM, PSRAM, `GDMA-AHB`, and `DW-GDMA`, while `Runtime::load_elf_in` explicitly invalidates `L1-ICache0` + `fence.i` across newly written executable segments (both in internal SRAM and external PSRAM) before user execution.
- `RegionAllocator` manages two disjoint NAPOT pools: internal SRAM (4 KiB page granularity) and the entire 32 MiB external AP HEX PSRAM (`0x88000000..0x8a000000`, 512 × 64 KiB MMU pages). When full-screen PSRAM display modes (`display diag`, `display boot`) are active, the display driver dynamically allocates a 2 MiB region from `RegionAllocator`, maps its 32 MMU pages, and marks it `InFlight` on `DW-GDMA CH0`; when the display is off or in internal-SRAM modes (`display test`, `display pixel`, `display orbit`), the 2 MiB framebuffer is unmapped and returned so applications can allocate up to the full 32 MiB of PSRAM.
- Userspace code is never linked into kernel `.text`/`.rodata`. `build.rs` compiles `vela-procfs`, `vela-shell`, and `vela-console` as stripped RV32IMAC static PIE ELFs and loads them at boot through the same `vela_kernel::elf::Image` validator, `R_RISCV_RELATIVE` relocator, and `RegionAllocator` used for runtime `ET_EXEC`/`ET_DYN` programs.
- Every process owns an `AddressSpace` of up to 6 disjoint W^X NAPOT regions (`READ`, `READ | WRITE`, or `READ | EXECUTE`) compiled into a slim cached PMP image (`pmpaddr0..5` + `pmpcfg0..1` over permanent `entry15` deny-all) activated before `mret`, with CSR readback on the first activation of each new `(ProcSlot, revision)`.

## Capabilities & Object Lifecycle

- Every process holds a 16-slot generational capability table (`Handle = generation << 8 | slot`). Global table indices never cross the syscall ABI.
- Kernel objects (`Object`) cover five classes:
  - `Endpoint { index, generation }` — Bounded 4-message, 96-byte FIFO queues and synchronous `IPC_CALL` / `IPC_REPLY` / `IPC_REPLY_RECV` endpoints.
  - `Memory { owner, region }` — NAPOT SRAM or PSRAM regions supporting one-level `READ` / `READ | WRITE` shared mappings (`MEM_GRANT`) and per-channel `InFlight` DMA ownership tracking.
  - `Event { index, generation }` — Managed interrupt-backed event slots (`EventTable`, up to 8 hardware CLIC lines) supporting `EVENT_WAIT(handle, timeout_ticks)`, `IRQ_ACK(handle)`, and dynamic `IRQ_BIND` / `IRQ_UNBIND` lifecycle.
  - `DmaChannel { index, generation }` — Managed DMA channel capabilities (`DmaTable`: `GDMA-AHB CH1` memory copy and `DW-GDMA CH0` DSI video scanout) enforcing controller-specific `MemoryClass` compatibility, 64-byte cache-line alignment, and `InFlight` region holds across active transfers.
  - `Authority(ProcessSpawn | Display | Sentinel | IrqControl | I2c)` — Explicit capability-gated control over process spawning, display scanout, LP Sentinel supervision, IRQ slot binding, and board-filtered I²C sensor transfers (`I2C_TRANSFER`). No syscall grants authority by checking a hardcoded `Pid`.
- `CAP_GRANT` and `MEM_GRANT` enforce one-level delegation (`GRANT` and `CONTROL` are never delegable). `CAP_REVOKE`, `IRQ_UNBIND`, `MEM_FREE`, `ENDPOINT_CLOSE`, and final thread exit run through `vela_kernel::lifecycle`, unmapping revoked shared memory, waking affected `Wait::Receive` threads with `-EFAULT`, failing orphaned RPC callers or event waiters with `-EPIPE`, deferring reuse of `InFlight` DMA regions until transfer completion, and automatically masking abandoned or revoked hardware IRQ lines at both CLIC and peripheral `INT_ENA`/`INT_CLR` before releasing `RegionAllocator` (and PSRAM MMU pages), DMA, and Sentinel wake leases.

## Scheduling, Deadlines & Synchronous RPC

- `Scheduler<C>` runs at 100 Hz (SYSTIMER target 0) with 4 priority levels (`0..=3`), round-robin rotation within equal effective priority, and bounded `O(TASKS²)` transitive priority inheritance across `Wait::Reply` chains.
- `IPC_CALL` rejects self-RPC and walks `Wait::Reply` chains up to `TASKS` to reject transitive RPC cycles with `-EDEADLK` before blocking. When the endpoint queue is empty and a server thread is already blocked in `Wait::Receive`, `try_direct_ipc_call` hands the payload and reply token directly to the server without staging through the endpoint queue, while `IPC_REPLY_RECV` replies and re-enters `Wait::Receive` in a single trap.
- All time-bounded waits (`Wait::Sleep`, `Wait::Reply`, `Wait::Event`, and generic `Wait::Device { domain, token, pointer }`) store their wakeup limit in `Thread::deadline: Option<Ticks>`, expired in a single `O(TASKS)` pass inside `Scheduler::tick`.

## Safety & Concurrency Invariants

- Single active HP core (`hart0`); `HP1` is held in reset before cache/SRAM setup.
- Machine interrupts remain disabled (`mstatus.MIE = 0`) throughout every Rust kernel path. Interrupts are enabled only in U-mode (`MPIE = 1`, `MPP = U`) or inside the dedicated assembly idle loop (`vela_idle_entry`, `MPP = M`), which holds no Rust references to `RUNTIME`.
- All user pointers are validated against the active `AddressSpace` permissions before `copy_from_user` / `copy_to_user_within` / `copy_between_users`. During `DMA_COPY`, `AddressSpace::dma_access` temporarily zeroes U-mode PMP permissions on the DMA page and `RegionAllocator` marks the buffer `InFlight` until the completion interrupt verifies transfer status.
- `Runtime` is the sole runtime owner of `Console` (except non-returning `fatal`/`panic` takeover) and `DisplayControl` (`psram::Controller` + `scanout::Scanout`); all raw SoC MMIO/ROM operations are `unsafe fn`, and BSP/SoC diagnostics are returned by value rather than instantiating second `Console` handles.
- HP↔LP Sentinel communication uses a single hardware mailbox request slot managed by `RequestLink`: at most one request owns `(ARG0, ARG1, REQUEST_SEQ, COMMAND)` until LP answers that exact `sequence`; deadline expiration transitions the link to `offline` (`-ETIMEDOUT`) with idempotent doorbell reassertion until recovery.
- Hardware cycle accounting (`CycleAccounting`) tracks per-thread and per-hart `user_cycles`, `kernel_cycles`, and `idle_cycles` via 64-bit `mcycle`, integrates wall-clock CPU capacity across `40 MHz` XTAL -> `180 MHz` CPLL transitions against the 16 MHz SYSTIMER, and measures comparator-to-ISR latency on runtime timer ticks.
