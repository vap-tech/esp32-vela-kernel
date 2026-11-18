# Vela v0.3

Space Flow bare-metal Rust microkernel, ESP32-P4 v1.3 / M5Stack Tab5.
Hart0 M-mode kernel; PMP-isolated U-mode procfs, shell and console tasks.
100 Hz priority round robin, bounded process/thread pools, no global heap or async runtime.

    ./scripts/build.sh
    ./scripts/flash.sh /dev/ttyACM0
    ./scripts/monitor.sh /dev/ttyACM0

Monitor Ctrl-R resets; Ctrl-C detaches. Shell: help, cat <path>, uptime, memtest.
Try cat /proc/loadavg and cat /proc/uptime. /proc/interrupts reports the 64-bit
counts of delivered SYSTIMER and USB Serial/JTAG RX interrupts on hart0. File lookup and formatting run in
userspace procfs, requested through IPC; no kernel VFS or hardcoded file output.

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

Hardware v0.3 smoke: blocked receive/wakeup, kernel WFI and timer wake,
shell IPC reads of loadavg/uptime, help and unknown-file response.
Logs are in ignored target/v03-smoke.log and target/v03-final.log.
First Light documents are historical. Boot entry, packer and fixed Flash
offsets are unchanged. Only SRAM task-stack reservations gained a third stack.
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

The shell launches a fixed-address ET_EXEC with `run hello` and a static PIE
with `run pic`. Building both programs requires clang with the RISC-V target
and ld.lld. The loader checks RV32 bounds, segment permissions and entry before
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

On Tab5 v1.3, `sentinel` shows the LP firmware version and startup epoch
acknowledged by HP. The build compiles a separate no_std RV32IMAC binary with
the target listed in rust-toolchain.toml. HP stalls LP, loads the binary at
0x50108000, sets the LP reset vector, clears stale mailbox interrupts and
starts LP. Only HP writes mailbox word 0 (epoch). LP writes word 1 (echo),
issues an I/O fence, then writes word 2 (versioned READY) to signal HP CLIC
line 20. HP checks the latched bit, event and epoch and acknowledges the IRQ.
The firmware then waits in WFI. Hardware smoke returned `Sentinel READY v1,
epoch 1` and the shell continued to serve uptime and loadavg. Heartbeat,
diagnostics and sleep supervision remain future protocol commands.
