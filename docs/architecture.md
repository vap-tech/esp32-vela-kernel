# Current architecture

Cargo workspace has two crates: `vela-boot-esp32p4` (binary and platform composition)
and `vela-kernel` (SoC-independent First Light identity function). Both are no_std.
The kernel only receives a `core::fmt::Write` sink. No external crates or allocator.

Boot includes the PAC, SoC console, board identity and architectural trap modules
by explicit paths. They are source modules, not empty wrapper crates.
Dependency direction is boot -> kernel, with no reverse dependency. PAC only uses core.
Watchdog, HP1 reset/clock and cache accesses required before Rust are defined in
boot/esp32p4/entry.S; the runtime PAC only exposes registers it actually uses.
No duplicate Rust watchdog/cache initialization is performed.

The BSP names M5Stack Tab5; display, touch, PSRAM, C6, Wi-Fi, audio and SD are untouched.
CPU/peripheral clocks and USB PHY are inherited from ROM, without PLL retuning.
Other SoC directories, ABI, userspace and tools are organizational boundaries,
not claims of implemented ports. Empty directories are not tracked by Git.

Interrupts stay disabled, HP1 stays in reset. First Light ends in a diagnostic
polling loop with `?` and `!`, not a scheduling implementation.
Trap code saves integer state and machine CSRs in a 144-byte aligned frame.
It cannot return or switch tasks. FP registers are not saved because no return
is possible. No FPU is enabled or used by this integer-only milestone despite
using the reference imafc ABI target.

Unsafe invariants: early assembly owns cache/clock/watchdog registers and both
stacks while interrupts are off and HP1 is reset. MMIO accesses are aligned raw
pointers with volatile semantics, never Rust references to registers. Console
ownership is unique in normal execution. Panic/trap paths abandon normal console
execution permanently and acquire the fatal sink; no resumable alias exists.
Fatal diagnostics require a valid stack, cannot handle stack corruption or nested
faults safely, and are not a hardened exception subsystem. No hidden mutable globals.
