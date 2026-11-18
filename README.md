# Vela v0.3

Space Flow bare-metal Rust microkernel, ESP32-P4 v1.3 / M5Stack Tab5.
Hart0 M-mode kernel; PMP-isolated U-mode procfs, shell and console tasks.
100 Hz priority round robin, bounded process/thread pools, no global heap or async runtime.

## Build and run

The build requires Rustup, Clang with RISC-V support, `espflash`
and Python 3. It uses system `ld.lld` or the Rust toolchain bundled linker.
The pinned toolchain in `rust-toolchain.toml` includes the HP
and LP targets. The build produces `target/firmware.bin` with the bundled
userspace programs and the separate LP Sentinel image.

```sh
./scripts/build.sh
./scripts/flash.sh /dev/ttyACM0
./scripts/monitor.sh /dev/ttyACM0
```

The flash script rebuilds the image and checks the configured Tab5 before
writing. The monitor uses Ctrl-R to reset and Ctrl-C to detach.

| Shell command | Result |
| --- | --- |
| `help` | List shell commands. |
| `uptime`, `cat /proc/uptime` | Read elapsed time from userspace procfs. |
| `cat /proc/loadavg` | Read sampled runnable demand. |
| `cat /proc/interrupts` | Read 64-bit SYSTIMER and USB Serial/JTAG RX interrupt counts on hart0. |
| `memtest` | Check aligned userspace allocation, bounds and release. |
| `run hello`, `run pic` | Launch the bundled fixed-address ELF and static PIE, respectively. |
| `run dma` | Copy and verify 2048 bytes of SRAM using GDMA-AHB. |
| `sentinel` | Show the LP protocol version and boot epoch acknowledged by HP. |
| `sentinel hello` | Send HELLO via the hardware mailbox and print the acknowledged sequence. |
| `sentinel status` | Read LP heartbeat state, age, counters and received missed notices. |
| `sentinel pause`, `sentinel resume` | Stop/restart HP heartbeats without stopping HP or resetting LP. |

## Kernel and userspace

IPC: endpoint_create/send/recv/endpoint_close, eight endpoints, four queued
messages each, max 96 bytes, sixteen generational capabilities per process. Creation
gives owner send+receive rights. Boot assignments and explicit capability grants give SEND to
peers; global endpoint IDs are not user handles. Endpoint owner alone receives.
Send is nonblocking: full => EAGAIN, no partial message. Services sleep before
retrying backpressure. Receive blocks or uses NONBLOCK; EMSGSIZE leaves the
message queued. Closing an owner handle destroys endpoint/queue and invalidates
all aliases; pending receives complete with EPIPE. Subsequent stale handles return
EBADF. Closing a peer SEND alias only releases that alias, not the endpoint.
Exit/fault cleans up owned endpoints and local aliases.

Queue check and wait registration are serialized with masked IRQs on hart0.
Send copies through kernel storage, completes the receiver's own saved return
register and then wakes it. mepc advances once, before blocking. All buffers
are validated against current mappings; writable buffers require a writable process mapping.
Explicit one-level memory grants support shared R/RW mappings and owner revocation.

Idle is a separate kernel WFI continuation, never a user task or loadavg input.
Timer wakes sleeping tasks; recv wakes on send. Kernel references do not survive
idle entry, and M-mode traps are resumable only from that assembly idle loop.
CLIC return keeps the active trap's bookkeeping and target privilege.

The console service drains USB Serial/JTAG RX in batches of up to 32 bytes.
Boot binds the RX event capability to the console process. EVENT_WAIT rearms
the packet interrupt and checks FIFO availability under masked hart interrupts
before blocking. CLIC line 18 wakes the driver on hart0. The handler masks and
acknowledges the packet source until the driver rearms it. The shared static
service image has an aligned 8 KiB RX mapping. On Tab5 v1.3 the shell accepted
help and both procfs reads with one serial write per command.

STATS accepts the original 44-byte little-endian snapshot and a 52-byte
version with a trailing USB RX interrupt count. Total counts live
tasks (including Blocked), runnable is Ready+Running; last ID includes exited
tasks. Idle is not in the task pool. Delivered timer IRQs and hardware uptime
ticks are separate. Q16 load averages sample runnable count every five seconds
with exp(-5/tau), tau=60/300/900 seconds; reads never reset accumulated values.
Load is sampled runnable demand, not CPU utilization: console wakeups can
contribute even when the CPU spends most time in WFI.

Processes own address spaces, cached PMP images and capability tables; threads
own saved contexts, stacks and priority/affinity. MEM_ALLOC reserves zeroed,
NAPOT-compatible owned SRAM; MEM_FREE revokes aliases before reuse. Endpoint
and memory grants cannot forward authority. IPC call/reply uses single-use reply
tokens, bounded priority donation, direct handoff and optional deadlines.

Userspace Arena explicitly suballocates aligned blocks inside one MEM_ALLOC
region; metadata stays on the service stack, with checked arithmetic and explicit
whole-region release. Raw pointers expire at release; sharing requires userspace
synchronization. No global allocator or individual free. `memtest` exercises
alignment, writable allocations, exhaustion and release in U-mode.
Tab5 v1.3 smoke: memtest, help, uptime and cat /proc/loadavg passed.
No SMP, POSIX, kernel VFS or dynamic IRQ registration.

## ELF and DMA

The ELF loader checks RV32 bounds, segment permissions and entry before
starting the process with a private stack and empty capability table. Exit and
fault release its SRAM. Each PIE receives one aligned backing allocation and
separate PMP mappings for its segments. The supported relocation is symbol-free
R_RISCV_RELATIVE with an aligned RW destination and an in-image addend.
The spawn ABI selects a bundled executable by index.

On Tab5 v1.3, `run dma` launches an isolated driver that checks a 2048-byte
SRAM copy byte by byte. GDMA-AHB channel 1 uses kernel-owned descriptors and
a linker-reserved page split into source and destination halves. The syscall
accepts only a word-aligned length of 4–2048 bytes from the owning PID.
CPU accesses use the direct uncached SRAM alias exclusively. Before starting
the transfer the kernel disables the user PMP mapping. The RX completion IRQ
checks descriptor ownership and length before restoring access and waking
the caller. Exit defers page reuse until completion. Failure or a one-second
timeout quarantines the page until reboot. The buffer supports internal SRAM
on this channel; PSRAM and cached user mappings require separate cache handling.

## LP Sentinel

Sentinel is a separate no_std RV32IMAC firmware in LP SRAM at 0x50108000.
It runs no HP code, reads no HP runtime memory and waits in WFI. Mailbox IRQ26
handles commands; LP TIMER target1 IRQ27 checks the exact heartbeat deadline.
This is supervision of an active HP, not light/deep-sleep wakeup support.

HP sends HEARTBEAT every second; LP uses its own 48-bit free-running counter
with a three-second timeout. Its inherited slow-clock frequency is measured
once against 16MHz SYSTIMER during ~20ms of stalled-LP startup. GET_STATUS
reports state, age_ms, calibrated lp_hz, accepted beats and missed transitions.
No heartbeat means unarmed until the first accepted heartbeat. A missed deadline
increments the transition count once and publishes HEARTBEAT_MISSED. A new
heartbeat restores healthy state without erasing counters. No automatic reset.

Commands: sentinel and sentinel hello retain READY/ACK diagnostics;
sentinel status requests a fresh LP snapshot; sentinel pause/resume disable or
enable only the HP heartbeat sender, for timeout/recovery checks. Status notices
counts asynchronous missed notifications actually received by HP. A blocked
status/HELLO request times out after one second; its transport slot stays
reserved until the matching response, so a late reply cannot complete a new call.

Wire version 2 uses scalar mailbox words only:
HP owns 0(epoch), 3(request seq), 4(command doorbell), 6/7(args),
13(LP publication ACK) and 15(calibrated Hz).
LP owns 1(echoed epoch), 2(event doorbell), 5(reply seq), 8(result),
9(state + missed count), 10/11(age ticks), 12(beat count), 14(publication seq).
Payload precedes each doorbell. HP copies and clears IRQ before acknowledging
the LP publication. LP never overwrites an unacknowledged response/event;
commands take precedence over deferred missed notices. Epoch and sequence are
checked before completion. Diagnostic journal and sleep supervision remain pending.

Tab5 v1.3 smoke (2026-11-18): healthy, pause -> missed=1/notices=1,
repeated GET_STATUS kept both at 1, resume -> healthy with growing beat count.
HELLO, help, uptime and /proc/loadavg passed. Observed calibrated slow clock:
130978 Hz; the value is measured anew at boot, not a platform constant.
Log: target/sentinel-heartbeat-smoke.log. No power-cycle or stress campaign.
