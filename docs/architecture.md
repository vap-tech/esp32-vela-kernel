# Vela Architecture

Vela is a `no_std` bare-metal Rust microkernel targeting ESP32-P4 v1.3 on M5Stack Tab5 (`riscv32imafc` HP hart0 + `riscv32imac` LP Sentinel). All kernel and userspace state uses bounded static tables with no global heap or async runtime.

## Workspace Layout (`src/`)

The Cargo workspace contains five crates with a strict acyclic dependency graph and zero cross-crate `#[path]` inclusions:

- `src/abi` (`vela-abi`) — Syscall numbers/constants and HP↔LP Sentinel wire protocol v2 definitions. Depends only on `core`.
- `src/kernel` (`vela-kernel`) — Architecture- and SoC-independent microkernel core: typed IDs (`Pid`, `Tid`, `ProcSlot`, `ThreadSlot`, `Handle`, `Ticks`), `Errno`, `capability`, `ipc`, `call` (`CallTable`), `memory` (`RegionAllocator`), `process` (`AddressSpace`, `Processes`), `sched` (`Scheduler`), `elf` (`ET_EXEC` and static PIE validator/relocator), `dma` (`Lease`), `sentinel` (`Heartbeat`, `WakeTicket`), `stats`, and cross-subsystem `lifecycle` orchestration. Pure `no_std` Rust; unit-tested on the host (`cargo test -p vela-kernel`).
- `src/lp` (`vela-lp`) — LP Sentinel core logic (`watch`, `journal`, `wakeup`, host-tested via `cargo test -p vela-lp`), shared MMIO helpers (`hw::{lp_counter, mailbox}`), and the standalone `vela-sentinel` RV32IMAC binary linked at LP SRAM `0x50108000`.
- `src/user` (`vela-user`) — Standalone U-mode runtime library (`sys`, `Buffer`, bump `Arena`) and isolated static PIE service binaries (`vela-procfs`, `vela-shell`, `vela-console`), plus assembly test programs (`apps/{hello,pic,dma}`).
- `src/platform` (`vela-boot-esp32p4`) — Two-stage SPM/SRAM bootstrap (`entry.S`), minimal ESP32-P4 PAC, CLIC trap entry (`arch/trap.S`), 16-entry NAPOT PMP compiler (`arch/pmp.rs`), SoC drivers (USB Serial/JTAG console, SYSTIMER, GDMA-AHB, DW-GDMA/DSI/ST7121/PSRAM display, LP loader), boot policy (`boot.rs`), and thin syscall dispatchers (`dispatch/`).

Dependency direction:
- `vela-boot-esp32p4 -> {vela-kernel, vela-lp, vela-abi}`
- `vela-user -> vela-abi`
- `vela-lp -> vela-abi`
- `vela-kernel` has zero internal crate dependencies.

## Memory Protection & Standalone ELF Services

- Two-stage boot loads `.entry` into 8 KiB SPM (`0x30100000`), disables watchdogs, holds HP1 in reset, initializes caches, verifies CRC32 of the upper SRAM payload (`0x4ff40000`), zeroes `.bss`, and enters `rust_main`.
- Userspace code is never linked into kernel `.text`/`.rodata`. `build.rs` compiles `vela-procfs`, `vela-shell`, and `vela-console` as stripped RV32IMAC static PIE ELFs and loads them at boot through the same `vela_kernel::elf::Image` validator, `R_RISCV_RELATIVE` relocator, and `RegionAllocator` used for runtime `ET_EXEC`/`ET_DYN` programs.
- Every process owns an `AddressSpace` of up to 6 disjoint W^X NAPOT regions (`READ`, `READ | WRITE`, or `READ | EXECUTE`) compiled into a cached 16-entry hardware PMP configuration activated before `mret`.

## Capabilities & Object Lifecycle

- Every process holds a 16-slot generational capability table (`Handle = generation << 8 | slot`). Global table indices never cross the syscall ABI.
- Kernel objects (`Object`) cover four classes:
  - `Endpoint { index, generation }` — Bounded 4-message, 96-byte FIFO queues and synchronous `IPC_CALL`/`IPC_REPLY` endpoints.
  - `Memory { owner, region }` — NAPOT SRAM regions supporting one-level `READ` / `READ | WRITE` shared mappings (`MEM_GRANT`).
  - `Event { index, generation }` — Interrupt-backed kernel event sources (`CONSOLE_RX`).
  - `Authority(ProcessSpawn | Display | Sentinel)` — Explicit capability-gated control over process spawning, display scanout, and LP Sentinel supervision. No syscall grants authority by checking a hardcoded `Pid`.
- `CAP_GRANT` and `MEM_GRANT` enforce one-level delegation (`GRANT` and `CONTROL` are never delegable). `CAP_REVOKE`, `MEM_FREE`, `ENDPOINT_CLOSE`, and final thread exit run through `vela_kernel::lifecycle`, unmapping revoked shared memory, waking affected `Wait::Receive` threads with `-EFAULT`, failing orphaned RPC callers with `-EPIPE`, and releasing `RegionAllocator`, DMA, and Sentinel wake leases.

## Scheduling, Deadlines & Synchronous RPC

- `Scheduler<C>` runs at 100 Hz (SYSTIMER target 0) with 4 priority levels (`0..=3`), round-robin rotation within equal effective priority, and bounded `O(TASKS²)` transitive priority inheritance across `Wait::Reply` chains.
- `IPC_CALL` rejects self-RPC and walks `Wait::Reply` chains up to `TASKS` to reject transitive RPC cycles with `-EDEADLK` before blocking.
- All time-bounded waits (`Wait::Sleep`, `Wait::Reply`, `Wait::Event`, and generic `Wait::Device { domain, token, pointer }`) store their wakeup limit in `Thread::deadline: Option<Ticks>`, expired in a single `O(TASKS)` pass inside `Scheduler::tick`.

## Safety & Concurrency Invariants

- Single active HP core (`hart0`); `HP1` is held in reset before cache/SRAM setup.
- Machine interrupts remain disabled (`mstatus.MIE = 0`) throughout every Rust kernel path. Interrupts are enabled only in U-mode (`MPIE = 1`, `MPP = U`) or inside the dedicated assembly idle loop (`vela_idle_entry`, `MPP = M`), which holds no Rust references to `RUNTIME`.
- All user pointers are validated against the active `AddressSpace` permissions before `copy_from_user` / `copy_to_user_within`. During `DMA_COPY`, `AddressSpace::dma_access` temporarily zeroes U-mode PMP permissions on the DMA page until the completion interrupt verifies transfer status.
