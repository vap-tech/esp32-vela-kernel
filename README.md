# Vela v0.3

Space Flow bare-metal Rust microkernel, ESP32-P4 v1.3 / M5Stack Tab5.
Hart0 M-mode kernel; PMP-isolated U-mode procfs, shell and console tasks.
100 Hz priority round robin, bounded process/thread pools, no global heap or async runtime.

    ./scripts/build.sh
    ./scripts/flash.sh /dev/ttyACM0
    ./scripts/monitor.sh /dev/ttyACM0

Monitor Ctrl-R resets; Ctrl-C detaches. Shell: help, cat <path>, uptime, memtest.
Try cat /proc/loadavg and cat /proc/uptime. File lookup and formatting run in
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

Console RX is deliberately bounded polling in a U-mode service: <=32 bytes per
sample, sleeps one 10ms tick between samples, with boot-assigned read authority.
This is not interrupt-driven USB input. Shell blocks on input IPC and reply IPC.
No USB polling or periodic console formatting occurs in the timer ISR.

Stats ABI remains an explicit 44-byte little-endian snapshot. Total counts live
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
The polling console can lose burst input; paced monitor input was used.
No SMP, DMA, Sentinel, POSIX, kernel VFS or IRQ driver framework.

The shell launches a fixed-address ET_EXEC with `run hello` and a static PIE
with `run pic`. Building both programs requires clang with the RISC-V target
and ld.lld. The loader checks RV32 bounds, segment permissions and entry before
starting the process with a private stack and empty capability table. Exit and
fault release its SRAM. Each PIE receives one aligned backing allocation and
separate PMP mappings for its segments. The supported relocation is symbol-free
R_RISCV_RELATIVE with an aligned RW destination and an in-image addend.
The spawn ABI selects a bundled executable by index.
