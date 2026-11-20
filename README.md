![Vela Microkernel](readme.svg)

# Vela

Bare-metal Rust microkernel, ESP32-P4 v1.3 / M5Stack Tab5.
Hart0 M-mode kernel; PMP-isolated U-mode procfs, shell and console tasks.
100 Hz priority round robin, bounded process/thread pools, no global heap or async runtime.
LP Sentinel independently observes HP heartbeats, keeps a diagnostic journal
and delivers one-shot timer events through the hardware mailbox.

## Build and run

The build requires Rustup, Clang with RISC-V support, `espflash`
and Python 3 with Pillow. It uses system `ld.lld` or the Rust toolchain bundled linker.
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
| `cat /proc/interrupts` | Read 64-bit hart0 interrupt counts (`systimer0`, `usb_serial_jtag_rx`, `gdma_ahb_ch1`, `lp_sentinel`). |
| `cat /proc/cpuinfo` | Read live hart0/APB/LP clocks, frequency-aware `mcycle` utilization (`user`/`kernel`/`idle`/`capacity`) and measured SYSTIMER IRQ latency. |
| `cat /proc/tasks` | List active threads with TID/PID, wait state, `base/effective` priority, ticks, measured `user_cycles`, preemptions and stack size. |
| `cat /proc/streams` | Read live DW-GDMA DSI scanout (`frames`, `fifo`, `underruns`, `dma_errors`) and GDMA-AHB CH1 transfer telemetry. |
| `bench` | Measure U-mode syscall, thread context switch and cross-process IPC `CALL`/`REPLY` round-trip in `mcycle` cycles plus IRQ latency. |
| `display diag` | Show the full-screen diagnostic image from PSRAM, in keyboard-down landscape. |
| `display boot` | Show the Vela boot artwork from the same PSRAM framebuffer. |
| `display pixel` | Draw a white pixel at (360, 640) on black using trusted kernel scanout. |
| `display test` | Identify the ST7121 via touch firmware, initialize the two-lane DSI link and enable vertical color bars and backlight. |
| `memtest` | Check aligned userspace allocation, bounds, capability lookup and release across both internal SRAM and external PSRAM. |
| `run hello`, `run pic`, `run pic-psram` | Launch the bundled fixed-address ELF (SRAM) and static PIE (SRAM or external PSRAM), respectively. |
| `run dma` | Copy and verify 2048 bytes of SRAM using GDMA-AHB. |
| `sentinel` | Show the LP protocol version and boot epoch acknowledged by HP. |
| `sentinel hello` | Send HELLO via the hardware mailbox and print the acknowledged sequence. |
| `sentinel status` | Read LP heartbeat state, age, counters and received missed notices. |
| `sentinel pause`, `sentinel resume` | Stop/restart HP heartbeats without stopping HP or resetting LP. |
| `sentinel log` | Read the bounded LP diagnostic journal without consuming records. |
| `sentinel mark` | Send an HP diagnostic MARK event with the current HP uptime tick as its value. |
| `sentinel wake <ms>` | Arm an LP timer and block until its WAKE_EVENT. Range: 1–60000 ms. |
| `sentinel prepare <ms>` | Agree a heartbeat pause with LP for 1–60000 ms while HP stays active. Use `sentinel cancel` or `sentinel wait` with the returned alarm. |
| `sentinel arm <ms>`, `sentinel wait` | Arm without waiting for expiry; later consume the latched event. |
| `sentinel cancel` | Cancel the current alarm before it fires; an already-fired event remains available to wait. |

The bars come from the DSI host pattern generator and require no framebuffer.
The command reports completion after PHY lock and command FIFO drain. Inspect
the glass to confirm scanout. Display errors 1 and 2 indicate expander and
touch I²C failures, 3 a PHY startup timeout, 4 a packet FIFO timeout and 5
loss of PHY lock. Values 256–511 encode an unsupported touch firmware byte
as 256 plus the version.

## Quick Sentinel session

Run these commands at the shell prompt:

```text
sentinel
sentinel status
sentinel mark
sentinel log
sentinel wake 1000
sentinel arm 10000
sentinel cancel
sentinel arm 10
sentinel wait
```

`wake` blocks until the actual LP event. `arm` returns after the setup ACK;
`wait` also accepts an event that has already arrived. Cancel before expiry;
if it has already fired, collect it with `sentinel wait`. There is one alarm slot.
Journal timestamps use calibrated LP ticks; `elapsed_ticks` uses HP 10ms ticks.
Neither command enters PMU light/deep sleep.

## Current scope

| Available | Current limit |
| --- | --- |
| Process/thread pools, priority scheduling, PMP and capabilities | Hart0 only; no HP SMP or enabled userspace FPU/DSP/PIE state. |
| ELF32 and static PIE loading (internal SRAM + external PSRAM) | Bundled images selected by index; no general filesystem loader or dynamic linker. |
| Managed SRAM (4 KiB) and PSRAM (64 KiB MMU, 30 MiB pool) allocation + controlled DMA copy | Flash XIP and general cached `L1-DCache` DMA are not enabled (`L1-ICache0` is enabled). |
| Managed IRQ/Event capability slots (`EVENT_WAIT`, `IRQ_ACK`, `IRQ_BIND`, `IRQ_UNBIND`) | Fixed platform IRQ slot table; currently wired for USB Serial/JTAG RX (CLIC line 18). |
| Sentinel heartbeat, journal and wake events | Volatile LP state; HP startup resets LP. No PMU sleep/resume integration. |
| Userspace shell and procfs | No kernel VFS or POSIX compatibility. |

Boot services (`procfs`, `shell` and `console`) are built as standalone static PIE
ELF binaries in `src/user` and loaded into private PMP-isolated SRAM allocations at
startup. `docs/architecture.md` documents the workspace layout and kernel invariants;
this README records the supported commands and hardware checkpoints.

## Kernel and userspace

IPC: endpoint_create/send/recv/endpoint_close, eight endpoints, four queued
messages each, max 96 bytes, sixteen generational capabilities per process. Creation
gives owner send+receive rights. Boot assignments and explicit capability grants give SEND to
peers; global endpoint IDs are not user handles. Receiving requires an explicit
process-local RECEIVE capability; handle values alone confer no authority.
Privileged operations (process spawning, display scanout control, Sentinel
supervision and IRQ slot binding/consumption) likewise require explicit process-local capability
handles granted by boot policy rather than ambient PID checks.
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
Boot registers CLIC line 18 in `EventTable` (`IRQ_SLOT_CONSOLE_RX = 0`) and binds
its event capability to the console process. `EVENT_WAIT(handle, timeout_ticks)`
supports bounded deadlines (`0` = infinite, `1..=6000` ticks -> `-ETIMEDOUT`),
rearms the packet interrupt and checks FIFO availability under masked hart interrupts
before blocking. CLIC line 18 wakes the driver on hart0: the handler masks and
acknowledges the hardware source at CLIC + peripheral `INT_ENA`/`INT_CLR` until
the driver rearms it via `EVENT_WAIT` or `IRQ_ACK(handle)`. Driver exit, `IRQ_UNBIND`
or `CAP_REVOKE` automatically masks the hardware line and wakes blocked waiters
with `-EPIPE`, preventing interrupt storms from abandoned level sources.
On Tab5 v1.3 the shell accepted help and procfs reads with one serial write per command.

STATS accepts the original 44-byte little-endian snapshot and a 52-byte
version with a trailing USB RX interrupt count. Total counts live
tasks (including Blocked), runnable is Ready+Running; last ID includes exited
tasks. Idle is not in the task pool. Delivered timer IRQs and hardware uptime
ticks are separate. Q16 load averages sample runnable count every five seconds
with exp(-5/tau), tau=60/300/900 seconds; reads never reset accumulated values.
Load is sampled runnable demand, not CPU utilization: console wakeups can
contribute even when the CPU spends most time in WFI.

Processes own address spaces, cached PMP images and capability tables; threads
own saved contexts, stacks and priority/affinity. `MEM_ALLOC(bytes, class)` reserves zeroed,
NAPOT-compatible owned internal SRAM (`MEM_CLASS_SRAM = 0`, 4 KiB pages) or external PSRAM
(`MEM_CLASS_PSRAM = 1`, 64 KiB MMU pages in `0x8820_0000..0x8a00_0000` beyond the 2 MiB display
framebuffer window); `MEM_FREE` revokes aliases and unmaps PSRAM MMU pages before reuse. Endpoint
and memory grants cannot forward authority. IPC call/reply (`IPC_CALL`, `IPC_REPLY`, `IPC_REPLY_RECV`) uses single-use reply
tokens, bounded priority donation, direct client-to-server handoff when a receiver is already waiting, and optional deadlines.

Userspace Arena explicitly suballocates aligned blocks inside one MEM_ALLOC
region; metadata stays on the service stack, with checked arithmetic and explicit
whole-region release. Raw pointers expire at release; sharing requires userspace
synchronization. No global allocator or individual free. `memtest` exercises
alignment, writable allocations, exhaustion and release in U-mode across both SRAM and PSRAM.
Tab5 v1.3 smoke: memtest, help, uptime and cat /proc/loadavg passed.
No SMP, POSIX or kernel VFS.

## ELF and DMA

The ELF loader checks RV32 bounds, segment permissions and entry before
starting the process with a private stack and empty capability table. Exit and
fault release its SRAM and unmap any owned PSRAM MMU pages. Each PIE receives one aligned backing allocation (in SRAM for `run pic` or in the managed PSRAM pool for `run pic-psram`) and
separate PMP mappings for its segments. The supported relocation is symbol-free
R_RISCV_RELATIVE with an aligned RW destination and an in-image addend.
The spawn ABI selects a bundled executable by index.

On Tab5 v1.3, `run dma` launches an isolated driver that checks a 2048-byte
SRAM copy byte by byte. GDMA-AHB channel 1 uses kernel-owned descriptors and
a linker-reserved page split into source and destination halves. The syscall
accepts only a word-aligned length of 4–2048 bytes from the owning PID.
Data accesses keep `L1-DCache` bypassed for direct uncached DMA coherency, while HP0's 16 KiB `L1-ICache0` accelerates instruction fetch and is invalidated across newly loaded ELF segments. Before starting
the transfer the kernel disables the user PMP mapping. The RX completion IRQ
checks descriptor ownership and length before restoring access and waking
the caller. Exit defers page reuse until completion. Failure or a one-second
timeout quarantines the page until reboot. The buffer supports internal SRAM
on this channel; PSRAM and cached user mappings require separate cache handling.

## LP Sentinel

Sentinel is a separate no_std RV32IMAC firmware in LP SRAM at 0x50108000.
It runs no HP code, reads no HP runtime memory and waits in WFI. Mailbox IRQ26
handles commands; LP TIMER target1 IRQ27 serves the earliest heartbeat/wake deadline.
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

If an automatic heartbeat receives no reply for one second, `sentinel` reports
a heartbeat timeout. HP retries the original sequence once per second. LP
acknowledges duplicate delivery without counting a second beat, and HP resumes
normal operation on the matching reply.

Wire version 2 uses scalar mailbox words only:
HP owns 0(epoch), 3(request seq), 4(command doorbell), 6/7(args),
13(LP publication ACK) and 15(calibrated Hz).
LP owns 1(echoed epoch), 2(event doorbell), 5(reply seq), 8(result),
9(state + missed count), 10/11(age ticks), 12(beat count), 14(publication seq).
Payload precedes each doorbell. HP copies and clears IRQ before acknowledging
the LP publication. LP never overwrites an unacknowledged response/event;
commands take precedence over deferred missed notices. Epoch and sequence are
checked before completion. PMU sleep entry and resume remain pending.

Tab5 v1.3 smoke (2026-11-18): healthy, pause -> missed=1/notices=1,
repeated GET_STATUS kept both at 1, resume -> healthy with growing beat count.
HELLO, help, uptime and /proc/loadavg passed. Observed calibrated slow clock:
130978 Hz; the value is measured anew at boot, not a platform constant.
Log: target/sentinel-heartbeat-smoke.log. No power-cycle or stress campaign.

### Diagnostic journal

Sentinel keeps 16 LP-local records per LP boot: sequence, 48-bit timestamp in
LP ticks since startup, 16-bit type and two u32 arguments. It records boot,
first heartbeat, timeout, recovery and HP diagnostics. Args are (epoch,hz)
for boot, (beat_count,timeout_ms) for first heartbeat, (beat_count,miss_count)
for timeout/recovery, and (code,value) for HP diagnostics.

The log header reports epoch, calibrated Hz, oldest/newest sequence, capacity,
overwritten-record count and received diagnostic-notice count. Reading is
nondestructive and limited to the captured tail. If a record is overwritten
between metadata and entry reads, the reader reports that loss explicitly.
Sequence IDs never wrap; exhaustion rejects new records without stopping
heartbeat observation. A pending diagnostic notice applies backpressure to new
HP diagnostics rather than replacing it. Replies and DIAGNOSTIC_EVENT use the
same acknowledged publication slot; no unacknowledged payload is overwritten.
The additive v2 entry payload packs timestamp_hi16 with type in one word;
see src/abi/src/sentinel.rs. No HP/LP shared RAM queue or pointers are introduced.

This is volatile LP SRAM, not persistent crash storage. Current HP startup
resets LP, so an HP reboot also starts a new journal/epoch.

Tab5 smoke: log -> mark -> log -> pause/timeout -> resume -> log showed
boot, first_heartbeat, hp_diagnostic, timeout and recovered with stable earlier
records; diagnostic_notices=1, missed=1/notices=1 and healthy after recovery.
Help, uptime and /proc/loadavg passed. Ring overwrite, packed timestamp wrap and
sequence exhaustion passed three host tests; overwrite was not stress-tested on
hardware. Log: target/sentinel-journal-smoke.log.

### Active-HP wake events

SET_WAKEUP arms one LP-local one-shot alarm; its token is the HP request
sequence, scoped to the boot epoch. The command ACK only confirms arming.
WAKE_EVENT is a separate acknowledged publication with token, actual LP firing
timestamp and configured delay. LP target1 arbitrates the nearest wake/heartbeat
deadline using the 48-bit counter. After programming, it rechecks expiry before
returning to WFI, so a deadline crossed inside the handler is serviced.

ARM/WAIT/CANCEL syscalls are process-owned and keep a single alarm slot.
An early event is latched until WAIT; condition check and registration occur
under masked hart0 IRQs. WAIT blocks a thread, never polls. It returns the token
and actual elapsed SYSTIMER 10ms ticks at event receipt, not the requested delay.
CANCEL succeeds only before LP observes expiry; a fired/pending event returns
EALREADY and must still be collected. A second waiter or uncollected alarm is
EBUSY. A safety timeout leaves the token reserved for a late event or cancellation;
it cannot complete a newly allocated alarm. Epoch/token checks and process IDs
prevent stale events waking unrelated processes or reused thread slots.

The LP journal includes wake_armed, wake_fired and wake_cancelled, with
(token,delay_ms) arguments. Wake notifications use the same payload/ACK protection
as other events and survive mailbox backpressure. This is an event delivered to
an active HP runtime, including ordinary kernel WFI. No PMU light/deep-sleep,
power-domain wake source or sleep-resume state has been enabled.

Tab5 smoke: wake 1000 returned elapsed_ticks=100; arm 10 followed by a later
wait returned the retained event with elapsed_ticks=1; arm 2000/cancel left no
pending event and no wake_fired record for that token. Heartbeat remained healthy
with missed=0; log, status, help, uptime and /proc/loadavg passed. Four host tests
cover one-shot/backpressure, cancellation, 48-bit wrap and deadline arbitration.
Log: target/sentinel-wakeup-smoke.log.

### Preparing a sleep interval

A healthy observer accepts PREPARE_SLEEP after reserving the one-shot timer.
Its ACK carries the wake token. HP suppresses automatic heartbeats for that
ticket, and STATUS reports `prepared` with the age of the last real heartbeat.
Cancellation before the deadline disarms the timer. At or after the deadline
expiry wins, and WAIT consumes one WAKE_EVENT. Either outcome opens a fresh
heartbeat timeout window. HP remains active throughout this protocol check.

On Tab5 a cancelled 800 ms interval left no pending wake after its original
deadline. A 4500 ms interval produced one event at 450 HP ticks. STATUS
changed from prepared to healthy, with zero missed transitions and notices.

### Display bring-up checkpoint

The vertical bars have been visually confirmed on Tab5. Backlight now uses
LEDC channel 0 at 39.06 kHz with 62.5% duty, checked through duty readback.

`display orbit` renders the 64×64 asset derived from `orbit.svg` into a
64-row RGB565 band centered on the 720×1280 display. Each frame uses three
DW-GDMA spans for black, the band and black again. The black spans repeatedly
read a 256-byte zero source block with 16-item source bursts. The driver keeps four frames of descriptors and
rearms consumed entries from the existing 100 Hz timer.

The image band occupies 92160 bytes, the zero block 256 bytes and twelve DMA
descriptors 768 bytes. The display
section occupies 93184 bytes within a 96 KiB linker reservation. This uses
internal SRAM. The coverage asset occupies another 4096 bytes in the firmware.
The raster is prepared once when the command starts.

Error 7 reports a backlight PWM readback timeout. The stream check reports
error 8 for insufficient frame progress or recurring FIFO underrun and leaves
the diagnostic stream available for inspection.

Regenerate the checked-in coverage asset using `/usr/bin/python3
tools/orbit_asset.py` with PyGObject, Pycairo and librsvg installed.
The display section starts at 0x4ff00000. Linker assertions bound its size
and keep the reservation below the boot stack at 0x4ff26000.


### Minimal pixel scanout

`display pixel` initializes ST7121 and enables one white RGB565 pixel at
(360, 640) on a black 720×1280 image. The DMA source uses a 256-byte zero block, one 64-bit pixel word
and twelve 64-byte descriptors (1032 bytes actively used). The shared
93184-byte display section also serves Orbit.
No additional pixel buffer is allocated.

Display initialization selects the fixed rev1.x CPU/MEM/SYS 180 MHz, APB
90 MHz operating point from a calibrated 360 MHz CPLL, following ESP-IDF
v5.5.2. The boot configuration inherited from ROM used XTAL at 40 MHz.
At that speed DPI 80 MHz repeatedly underruns. With the higher bus rate,
the minimal source transmitted 40 frames during the startup check without
a recurring underrun. The white point was visually confirmed during
bring-up, and shell, procfs and Sentinel remained operational.

DPI stays at the visually confirmed 80 MHz. Stream startup preloads the
bridge FIFO. After startup settling, the command clears the underrun latch
and checks another 500 ms of scanout. Error 9 indicates clock setup failure.
The fixed clock point persists after the display command. Runtime DVFS and
power-state transitions require separate integration.
Hardware log is `target/display-pixel-smoke.log`.


Tab5 Orbit smoke passed 40 frames without recurring FIFO underrun at DPI
80 MHz and CPU/MEM 180 MHz. Shell, procfs and Sentinel remained operational.
Hardware log is `target/display-orbit-smoke.log`.


The centered Orbit image and the black background were visually confirmed
on Tab5. An initial single-word black source produced vertical artifacts
outside the image band despite a clear underrun latch. A 256-byte zero
source and 16-item source bursts removed those artifacts. Pixel and Orbit
share the same zero source.


### Full-screen graphics in PSRAM

`display diag` and `display boot` use the PNG masters in
`vela_boot_and_display_assets`. During the build, `tools/display_assets.py`
rotates each 1280×720 PNG counterclockwise into the 720×1280 scanout layout,
placing the keyboard connector along the bottom edge,
quantizes it to RGB565 and encodes little-endian (u16 count, u16 pixel)
runs. The converter checks its output by an exact round trip. The run streams
are packaged into a Flash-only area at 0x100000. Cargo includes their addresses,
lengths, CRCs and expected header as small metadata.

The trusted driver powers LDO2 at 1.8 V and the shared MSPI PHY, reads AP
mode registers and verifies the selected chip's 32 MiB density. The link
initializes at SPLL/12 = 40 MHz and uses SPLL/6 = 80 MHz for HEX x16 DDR.
The first 2 MiB (`pages 0..32`) are identity-mapped through the PSRAM controller MMU.
A single framebuffer occupies 1843200 bytes at physical 0x48000000.
CPU rendering and verification use the uncached 0x88000000 alias.
DW-GDMA reads the bus alias with one descriptor per frame and a four-frame
ring. The display driver exclusively owns this 2 MiB window (`0x88000000..0x88200000`), while the
remaining 30 MiB (`0x88200000..0x8a000000`, `pages 32..512`) are managed as a 64 KiB-page NAPOT
PSRAM pool by `RegionAllocator` (`MEM_CLASS_PSRAM` and `run pic-psram`).

The raster is expanded and read back in batches of 32 rows, yielding between
batches. The framebuffer is written only after the previous scanout stops.
Both full-screen images and their orientation were visually confirmed on
Tab5. Startup checks passed 40 frames without recurring underrun, followed
by shell/procfs, AHB DMA copy and healthy Sentinel with missed=0.
Logs for Flash-backed rendering are `target/display-flash-diag.log` and
`target/display-flash-boot.log`.

The RLE assets occupy 87948 + 183908 bytes in Flash, with a 64-byte
versioned header. The build appends this area to the normal firmware image
and checks Flash bounds, padding, stream pixel counts and CRCs. The runtime
compares the header with the compiled metadata and checks the selected image's
CRC before rendering. Source reads use a 4 KiB aligned stack buffer, reused
for expansion and framebuffer readback. A separate validation reader also
uses 4 KiB during the first row batch.

Current SRAM payload is 117416 bytes (including bundled service and test ELFs),
SPM `.entry` (bootstrap + trap entry/exit) is 1352 bytes, and kernel BSS is 11136 bytes, leaving 298 KiB before the separate AHB DMA
reservation. This frees 258 KiB compared with
embedding the two images in SRAM. The ordinary SRAM Orbit buffer remains
reserved for mode switching. The artwork is selected through the shell;
startup still enters the USB shell.

Display error codes 10–12 indicate PSRAM vendor/density/probe failure,
13 an invalid image stream/range, 14 a framebuffer readback mismatch and
15 a PSRAM command timeout, 16 a Flash read/range error, 17 an asset
header mismatch and 18 an image CRC mismatch. Cached `L1-DCache` PSRAM mappings and Flash XIP remain separate work.
