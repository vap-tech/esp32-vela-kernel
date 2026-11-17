# Vela

Space Flow bare-metal Rust microkernel on ESP32-P4 v1.3 / M5Stack Tab5.
Current code: hart0 M-mode kernel, two U-mode task instances with private 4 KiB
stacks, shared RX user code, PMP isolation, 100 Hz SYSTIMER/CLIC round robin.
Tasks run computation without yield; only timer interrupts switch them by default.
No allocator, async runtime, SMP, IPC, VFS or shell.

```sh
./scripts/build.sh
./scripts/flash.sh /dev/ttyACM0
./scripts/monitor.sh /dev/ttyACM0
```

Rust toolchain is pinned; host needs rustup, Python 3 and espflash 4.7.0.
Monitor: Ctrl-R resets, Ctrl-C detaches. Bring-up commands affect the currently
running task: `a` enters a U-mode syscall probe (stats/bad pointer/yield/exit),
`k` attempts a kernel SRAM read, `m` attempts USB MMIO, `s` attempts peer-stack read.
Faulting tasks terminate; their peers continue. With no runnable tasks the kernel
halts; an idle task and blocking/wakeup paths are not implemented.

ABI is in abi/syscall.rs: write/yield/exit plus ticks and stats. STATS returns an
explicit 44-byte little-endian snapshot, not an internal kernel struct or pointer.
WRITE validates the whole buffer against current user mappings (128-byte limit),
returns the actual submitted count, and never waits indefinitely for USB.
User code is in userspace/first_tasks.rs; init/worker share the same fixed image
with distinct role arguments, contexts and stacks. All user data is stack-local.
FP is disabled; FP instructions fault rather than leaking unsaved FP state.

Scheduler statistics are separate from scheduling policy in kernel/src/stats.rs.
Runnable means Ready+Running, total means live tasks including Blocked, last ID
includes terminated tasks. Scheduler ticks count delivered timer interrupts;
uptime ticks come from the actual hardware counter in 10ms units since timer start.
These can differ after pending/coalesced interrupts. Every 5 seconds, Q16 EWMA
samples runnable tasks with exp(-5/tau), tau=60/300/900 seconds; initial value is zero.
The `load_q16` diagnostic contains actual Q16 values, not decimal /proc output.

Hardware smoke passed on 2026-11-17: U-mode ecalls, both no-yield tasks preempted,
PMP denial of kernel SRAM/MMIO/peer stack, fault termination with peer continuing,
write/stats/bad-pointer/yield/exit ABI probe. Logs: target/multitasking-abi.log and
target/multitasking-active-cause.log. No readback, power-cycle or stress test was done
for this milestone. Boot entry.S is unchanged; old First Light docs describe history.

The return path is specific to P4's CLIC: saves all integer registers, mepc,
mstatus, raw mcause, uses mscratch to enter a kernel stack, and restores the live
trap's CLIC bookkeeping on task selection. Every user return resets previous
machine interrupt level to zero; kernel interrupts are never nested. Kernel faults
halt. Fatal faults during stack corruption/return are not a hardened recovery path.
PMP CSR readback is checked on activation; this is memory isolation, not a security audit.

Next: kernel idle task and block/wakeup semantics, then user-facing syscall ABI
library and a userspace procfs service; no fictitious file operations are provided.
