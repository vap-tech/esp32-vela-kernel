![Vela Microkernel](readme.svg)

# Vela

Bare-metal Rust microkernel, ESP32-P4 v1.3 / M5Stack Tab5.
Dual-core HP0+HP1 SMP M-mode kernel; PMP-isolated U-mode `sysd`, `init`, `console`, `fbcon`, `kbd` and `ext4fs` services with full RV32F FPU + ESP32-P4 v1.3 PIE (`Xespv2p1`) context switching.
100 Hz priority round robin, bounded process/thread pools, no global heap or async runtime.
LP Sentinel independently observes HP heartbeats, keeps a diagnostic journal
and delivers one-shot timer events through the hardware mailbox.

Application development is documented in [the SDK guide](docs/sdk.md).
`./scripts/build-app.sh` builds an independent `no_std` Rust example using only
`vela-sdk`, ready to copy to microSD and run through shell, pipes or redirection.

Sequential ext4 reads use delegated IPC bulk windows, with one data copy
from private SD DMA SRAM into the client. Client requests of 64 KiB are
served through SD operations of up to 4 KiB. The instrumented Tab5 test
reached 1.59 MiB/s. The next experiment measures the ordinary file path
with detailed profiling enabled only on request. Measurements and their
limits are recorded in [the roadmap](docs/roadmap.md).

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
| `help` | List shell commands and virtual namespaces. |
| `ls [path]` | List `/`, `/proc`, `/sys`, `/sys/sentinel`, `/dev`, or any `/sd/<dir>` via `/sd/bin/ls.elf`. |
| `cat <path>` | Read any file in `/proc`, `/sys`, `/dev`, or `/sd` via `/sd/bin/cat.elf`. |
| `echo <val> [> <path>]` | Print text, or write control values to writable `/sys/*` and `/dev/*` nodes. |
| `uptime` | Run `/sd/bin/uptime.elf` (or `cat /proc/uptime`). |
| `run <elf> [arg]` | Run an RV32 static PIE ELF from `/sd` (up to 1 MiB) and wait for exit. |
| `sdexport [seconds \| rw]` | Run `/sd/bin/sdexport.elf` to export the microSD card over USB-C (read-only timeout or writable until host eject). |
| `exit` | Exit the shell (`init` automatically respawns it). |

Everything in userspace shares one File IPC protocol (`OP_READ`, `OP_LIST`, `OP_WRITE`) mounted across `sysd` (`/proc`, `/sys`, `/dev`) and `ext4fs` (`/sd`):

| Virtual path | Access | Description |
| --- | --- | --- |
| `/proc/uptime`, `/proc/loadavg` | R | Elapsed time and 1/5/15 min sampled runnable demand. |
| `/proc/interrupts` | R | Interrupt and SMP counters (`systimer0`, `usb_serial_jtag_rx`, `gdma_ahb_ch1`, `lp_sentinel`, `ipi_hp0`, `ipi_hp1`, `pmp_shootdowns`, `kbd_gpio50`). |
| `/proc/cpuinfo` | R | Live HP0+HP1/APB/LP clocks, `rv32imafc_xesppie` ISA, DVFS policy/switch counts, tickless sleep counters, per-hart `mcycle` utilization, IPI/PMP shootdowns and SYSTIMER IRQ latency. |
| `/proc/last_fault` | R | Most recent U-mode fault with PID/TID, decoded cause, PC, fault address, SP, stack bounds and guard-hit flag; retained across service restarts until reboot. |
| `/proc/kernel_stacks` | R | HP0/HP1 usable stack bytes, deepest observed write since boot, untouched bytes and boundary marker status. See [measurement limits](docs/memory-map.md#kernel-stack-checks-and-measurement). |
| `/proc/tasks` | R | Active threads with TID/PID, wait state, `base/effective` priority, `hart/aff`, migrations, ticks, `user_cycles`, preemptions and stack size. |
| `/proc/streams`, `/proc/sdcard`, `/proc/i2c` | R | DW-GDMA/GDMA-AHB stream counters, microSD status/capacity/counters, and board I²C bus definitions. |
| `/sys/power` | R | Live Tab5 `INA226` (`0x41`, `5 mΩ` shunt on `GPIO31`/`GPIO32`) 2S battery-rail voltage, shunt voltage, current, power and USB/battery supply state. |
| `/sys/freq` | R/W | Query or set HP0 DVFS operating-point policy (`echo auto\|40\|180 > /sys/freq`; `40` returns busy while display or DMA is active). |
| `/sys/display` | R/W | Read display flags (`blanked`, `sd_export`) or control scanout/backlight (`echo off > /sys/display`, `echo 0..255 > /sys/display`). |
| `/sys/sentinel/status`, `/sys/sentinel/log` | R | Read LP Sentinel heartbeat state/age/counters and bounded LP diagnostic journal. |
| `/sys/sentinel/sleep` | W | Coordinated heartbeat-paused sleep window (`echo <1..60000> > /sys/sentinel/sleep`; requires display/DMA off). |
| `/dev/i2c-0`, `/dev/i2c-1` | R | Address-only I²C bus scan (system bus `GPIO31`/`GPIO32`, Ext.Port1 `GPIO0`/`GPIO1`; Tab5 Keyboard answers at `0x6d` on bus 1). |
| `/dev/kbdled` | R/W | Read or control Tab5 Keyboard RGB LEDs (`echo auto\|off\|bright N\|R G B [R G B] > /dev/kbdled`). |
| `/dev/random`, `/dev/urandom`, `/dev/zero` | R | Hardware TRNG / ChaCha20 CSPRNG diagnostics and zero stream. |

## Quick Sentinel session

Run these commands at the shell prompt:

```text
cat /sys/sentinel/status
cat /sys/sentinel/log
echo off > /sys/display
echo 1500 > /sys/sentinel/sleep
```

Writing milliseconds (`1..60000`) to `/sys/sentinel/sleep` prepares a coordinated heartbeat-paused
sleep window with LP, enters tickless `wfi` (downshifting HP0 to `40 MHz` XTAL in `auto` mode)
after re-checking pending console/mailbox/IRQ events under `MIE=0`, and restores `180 MHz`
and periodic heartbeats upon wake.
Journal timestamps use calibrated LP ticks; `elapsed_ticks` uses HP 10ms ticks.

## Current scope

| Available | Current limit |
| --- | --- |
| Dual-core HP0+HP1 SMP, priority scheduling, thread affinity/migration, synchronous IPI PMP shootdown, capabilities, and per-thread `RV32F` FPU + `ESP32-P4 v1.3` PIE (`Xespv2p1`) extended context | Static 16-process / 16-thread pools; eager FPU+PIE save/restore on thread switch (`vela_ext_save`/`vela_ext_restore`). |
| ELF32 and static PIE loading (internal SRAM + external PSRAM) | Bundled boot services in flash plus runtime `/sd/bin/*.elf` loading via `SPAWN_IMAGE` / `SPAWN_IMAGE_WAIT` (up to 1 MiB, up to 4 passed capability handles); no dynamic linker. |
| Managed SRAM (4 KiB) and unified 32 MiB PSRAM (64 KiB MMU) pool + capability DMA (`DmaTable`) | Flash XIP and general cached `L1-DCache` DMA are not enabled (`L1-ICache0` and `L1-ICache1` are enabled). |
| Managed IRQ/Event capability slots (`EVENT_WAIT`, `IRQ_ACK`, `IRQ_BIND`, `IRQ_UNBIND`) | Fixed platform IRQ slot table; currently wired for USB Serial/JTAG RX (CLIC line 18). |
| Userspace `INA226` power driver (`I2C_TRANSFER`, `/sys/power`) | Scoped by BSP policy to `0x41` on Tab5 system I²C (`GPIO31`/`GPIO32`); board expanders stay kernel-owned. |
| Userspace Tab5 Keyboard driver (`vela-kbd`, `I2C_TRANSFER` on Ext.Port1 bus 1) | Sleeps on the keyboard `INT` line (`GPIO50`, level interrupt, `IRQ_SLOT_KBD = 1`, CLIC line 22), drains the `0x6d` Character-mode FIFO when it fires, then writes `INT_STA = 0` so the keyboard releases the line, and feeds the shell input endpoint next to the USB console; named keys (`enter`, `del`, arrows...) become terminal bytes and the shell drops ANSI escape sequences. `echo <auto|off|bright N|R G B [R G B]> > /dev/kbdled` drives the two RGB LEDs (registers `0x03`, `0x11`, `0x60`, `0x64`). No polling; `kbd_arm_low` / `kbd_arm_wait` in `/proc/interrupts` show how often the line was re-armed. After 5 minutes without a key the service dims the LEDs (brightness register `0x03` to 0) and restores them on the next key; while the screen is blanked (`STATS_DISPLAY`) the first key is swallowed and only wakes the display, via a no-op `ESC [ m` written to the console. |
| Userspace framebuffer console (`vela-fbcon`, `CONSOLE_LOG`, `DISPLAY_MODE_CONSOLE`) | Owns a PSRAM framebuffer and mirrors everything written to the system console (shell echo, kernel log) on the Tab5 screen, 80x22 cells with a 16x32 Terminus font (SIL OFL). After 5 minutes without console output the backlight is switched off (`DISPLAY_MODE_BACKLIGHT`); any new output, such as a key echo, wakes it. The renderer understands ANSI SGR colours (16-colour palette, bold, reset); the shell prompt is green and errors are red. While blanked it polls the log every 100 ms instead of 20 ms. While the display is attached `/sys/sentinel/sleep` and `echo 40 > /sys/freq` return `-EBUSY`. |
| Per-bus I²C capabilities (`Authority::I2c(bus)`) and probe scan (`/dev/i2c-N`) | Address-only probes work on every address of both buses; data transfers are limited to `0x41` on the system bus and open on Ext.Port1 (no board devices). |
| Tab5 microSD driver (`LDO_VO4` 3.3 V, `GPIO39..44`, `CMD59` CRC7 + `CRC16-CCITT`, `/proc/sdcard`) | Legacy raw assets at LBA 2048 and partition-bounded reads and writes for the U-mode ext4 service, including delegated bulk-read destinations. |
| Coordinated LP Sentinel sleep (`sentinel sleep`), DVFS (`40 MHz <-> 180 MHz`) and Tickless idle (`SYSTIMER`) | WFI + XTAL downshift with pre-sleep IRQ/generation re-check; full PMU HP power-domain power-down/reset resume is not used. |
| Userspace `sysd` (`/proc`, `/sys`, `/dev`), `ext4fs` (`/sd`), flash `init` (`boards/tab5/init.conf`), and `/sd/bin/shell.elf` | No kernel VFS or POSIX compatibility. |

Boot services (`sysd`, `init`, `console`, `fbcon`, `kbd`, `ext4fs`) are built as standalone static PIE
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
supervision, IRQ slot binding/consumption, DMA channel transfers and board I²C sensor access) likewise require explicit process-local capability
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
(`MEM_CLASS_PSRAM = 1`, 64 KiB MMU pages across the full 32 MiB pool `0x8800_0000..0x8a00_0000`);
`MEM_FREE` rejects regions with active `InFlight` DMA (`-EBUSY`), revokes aliases and unmaps PSRAM MMU pages before reuse. Endpoint
and memory grants cannot forward authority. IPC call/reply (`IPC_CALL`, `IPC_REPLY`, `IPC_REPLY_RECV`) uses single-use reply
tokens, bounded priority donation, direct client-to-server handoff when a receiver is already waiting, and optional deadlines.

Userspace `Arena` explicitly suballocates aligned blocks inside one `MEM_ALLOC`
region; metadata stays on the service stack, with checked arithmetic and explicit
whole-region release. Raw pointers expire at release; sharing requires userspace
synchronization. No global allocator or individual free.
Dual-core HP0+HP1 SMP with synchronous IPI PMP shootdowns and full FPU+PIE context switching is active; no POSIX or kernel VFS.

## ELF and DMA

The ELF loader checks RV32 bounds, segment permissions and entry before
starting the process with a private stack and explicitly granted capability table. Exit and
fault release its SRAM and unmap any owned PSRAM MMU pages. Each static PIE receives one aligned backing allocation (in SRAM or in the managed PSRAM pool) and
separate PMP mappings for its segments. The supported relocation is symbol-free
`R_RISCV_RELATIVE` with an aligned RW destination and an in-image addend.
Runtime binaries are loaded from `/sd/bin/*.elf` via `SPAWN_IMAGE` / `SPAWN_IMAGE_WAIT` with up to 4 attenuated capability handles.

`DmaTable` unifies `GDMA-AHB CH1` (memory copy)
and `DW-GDMA CH0` (MIPI-DSI scanout), validating controller-specific `MemoryClass`
compatibility (`GdmaAhb` -> `Sram`, `DwGdmaAxi` -> `Sram | Psram`), 64-byte
cache-line alignment, and per-channel `InFlight` region holds in `RegionAllocator`.
Data accesses keep `L1-DCache` bypassed for direct uncached DMA coherency, while HP0's 16 KiB `L1-ICache0` accelerates instruction fetch and is invalidated across newly loaded ELF segments. Before starting
the transfer the kernel disables the user PMP mapping and marks the region `InFlight`. The RX completion IRQ
checks descriptor ownership and length before restoring access, clearing `InFlight` and waking
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

Userspace interfaces: `cat /sys/sentinel/status` requests a fresh LP snapshot,
`cat /sys/sentinel/log` reads the LP diagnostic journal, and
`echo <ms> > /sys/sentinel/sleep` enters a coordinated heartbeat-paused sleep window
(the underlying kernel syscalls also retain HELLO, pause/resume, mark, and wake-alarm
operations). Status `notices` counts asynchronous missed notifications actually
received by HP. A blocked status/HELLO request times out after one second; its
transport slot stays reserved until the matching response, so a late reply cannot
complete a new call.

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

### Preparing a sleep interval, DVFS and Tickless Idle

A healthy observer accepts PREPARE_SLEEP after reserving the one-shot timer.
Its ACK carries the wake token. HP suppresses automatic heartbeats for that
ticket, and STATUS reports `prepared` with the age of the last real heartbeat.
`SENTINEL_PREPARE_SLEEP` rejects sleep preparation with `-EBUSY` while any DMA
channel (`GDMA-AHB CH1` or `DW-GDMA CH0` scanout) or the MIPI-DSI display
pipeline is active, or when an unconsumed IRQ/Event notification is pending in
`EventTable`; run `echo off > /sys/display` first to stop scanout and release the
framebuffer. Cancellation before the deadline disarms the timer. At or after the deadline
expiry wins, and WAIT consumes one WAKE_EVENT. Either outcome opens a fresh
heartbeat timeout window and immediately schedules a fresh HP heartbeat so LP
exits `PreparedSleep` without false missed-heartbeat transitions.

Right before entering `wfi`, `Runtime::poll_pending_before_idle` re-checks the
USB Serial/JTAG RX FIFO, the LP Sentinel mailbox doorbell (`has_pending_irq()`),
`EventTable::any_pending()` and `EventTable::table_generation()` under `MIE=0`.
If any thread is woken, the scheduler dispatches it immediately without entering
idle. Otherwise `Runtime::enter_tickless_idle` computes the next deadline across
thread sleeps/timeouts (`Scheduler::nearest_deadline`), Sentinel heartbeat/alarm
timeouts (`SentinelClient::next_deadline`) and the 5-second load average window
(`1..=400` ticks = 10 ms .. 4.0 s) and reprograms `SYSTIMER` comparator 0 in
26-bit period mode when neither display scanout nor DMA is active.

In `auto` DVFS mode (`CLOCK_CONTROL`, `echo auto|40|180 > /sys/freq`), HP0 downshifts to
`40 MHz` XTAL (`CPU/MEM/SYS/APB = 40 MHz`) during tickless and `/sys/sentinel/sleep`
windows when `!display.is_active() && !dma.any_active()`, and restores the active
`180 MHz` CPLL (`CPU/MEM/SYS = 180 MHz`, `APB = 90 MHz`) operating point upon wakeup.
`CycleAccounting` integrates exact `mcycle` capacity across every frequency switch
and tracks `freq_mode`, `freq_switches`, `tickless_sleeps` and `tickless_ticks` in
`/proc/cpuinfo`.

On Tab5 a cancelled 800 ms interval left no pending wake after its original
deadline. A 1500 ms `sentinel sleep 1500` interval downshifted HP0 from 180 MHz
to 40 MHz during sleep, woke on `WAKE_EVENT` at `elapsed_ticks=150`, restored
180 MHz and resumed heartbeats with `missed=0`.

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


### Display, PSRAM and microSD (SDMMC 4-bit IDMAC)

`vela_boot_and_display_assets` and `tools/display_assets.py` are retained in the
repository for future boot/background wallpapers: during the build, each 1280×720
PNG is rotated counterclockwise into the 720×1280 scanout layout (keyboard connector
at the bottom), quantized to RGB565, RLE-encoded, verified by exact round-trip, and
packed into the 271920-byte `VELAIMG1` bundle at Flash `0x100000`.

The microSD driver (`src/platform/src/soc/sdcard.rs`) enables internal PMU
`LDO_VO4` (`ext_ldo[4]`, `0x501151d8`/`0x501151dc`) at 3.3 V to power the
`VDDPST_5` (`GPIO39..48`) domain and drives the hardware 4-bit SDMMC host controller
(`SDHOST` at `0x50083000`, DesignWare DWC-MSHC) on Slot 0 (`GPIO39..44`) with an
8-descriptor chained IDMAC ring (32 KiB / 64 sectors per `CMD18`/`CMD25` multi-block
transfer at up to 40 MHz High-Speed), with automatic fallback to SPI mode (`CMD59`
CRC7 + software `CRC16-CCITT`).

The trusted display driver powers LDO2 at 1.8 V and the shared MSPI PHY, verifies the
32 MiB AP HEX PSRAM density, and switches the link to 80 MHz HEX x16 DDR (`SPLL/6`).
All 32 MiB (`0x88000000..0x8a000000`, `pages 0..512`) are managed as a unified
64 KiB-page NAPOT PSRAM pool by `RegionAllocator` (`MEM_CLASS_PSRAM`).
If `sysd` exits, pending namespace calls fail and its request queue is discarded. The kernel reserves the namespace endpoint until `init` starts its replacement. Existing client handles then reach the new `sysd` with their original rights, and the live SD service supplies `/sd`. The replacement also restores up to seven dynamic mounts and obtains fresh send handles to their live drivers. Driver memory survives the namespace restart. Unmounting, revoking the endpoint, or exiting the driver removes its saved registration.

If `ext4fs` exits, outstanding SD requests can fail. After 100 ticks `init` starts a fresh instance, which registers a new endpoint at the reserved `/sd` mount in the running `sysd`. Clients can issue new requests using their existing namespace handles. Interrupted writes are not replayed.

A separate `init` thread supervises the Tab5 keyboard. After driver exit it waits 100 ticks, then starts a fresh instance with an Ext.Port1 I²C capability, a new GPIO50 event binding, and send access to the existing shell input endpoint.

An `init` thread starts the bundled framebuffer console and waits for its process to exit. On exit the kernel detaches its scanout before reclaiming PSRAM, and `init` retries after 100 ticks. The small `fbcon` image, text grid, font and guarded 4 KiB stack run in SRAM. Its 2 MiB framebuffer remains in PSRAM. Running the renderer image in uncached PSRAM made redraws much slower. Startup rendering is measured in 10 ms system ticks, which remain valid across HP migration.

At startup the console requests the boot artwork from Flash in its own scanout buffer. After collecting the early console log it composes one native RGB565 row (1,440 bytes) at a time and copies each finished row over the artwork in scan order. The backlight stays on throughout the handoff. Scanout can still show a mixture of old and new rows during the transition. During the handoff on Tab5 at 180 MHz, writes use 16-byte bursts separated by a write fence and 200 NOPs. Three measured boots each took 350 ms with zero observed underruns, versus 290 ms and six underruns with whole-row copies. Later updates redraw changed cells.

The experimental console wallpaper uses a dimmed 320×180 sample of the boot
image (115,200 bytes of RGB565 data in PSRAM). Switch it with
`echo wallpaper > /sys/display` or `echo black > /sys/display`.
Idle log polls skip the 80×22 grid scan. Incremental rendering skips unchanged pixel pairs when replacing glyphs.
On Tab5, four scrolls of a filled console with 70 characters per line took
460 ms with wallpaper and 240 ms on black (previously 980 and 800 ms).

At boot, `vela-fbcon` allocates a 2 MiB framebuffer from `RegionAllocator`, initializes
the ST7121 panel and MIPI-DSI video pipeline (`DW-GDMA CH0`, 4-frame descriptor ring
reading the `0x48xx_xxxx` bus alias), and renders the 80×22 ANSI console. Writing
`echo off > /sys/display` stops scanout, disables backlight, releases the `InFlight`
DMA hold, and returns the 2 MiB region to the 32 MiB pool.

The local release snapshot of 2026-11-27 has a 253,916-byte SRAM payload
(including bundled service ELFs), a 1,728-byte SPM `.entry` and 41,328 bytes
of kernel BSS. Build scripts report the sizes for each new image.

Display error codes 10–12 indicate PSRAM vendor/density/probe failure,
13 an invalid image stream/range, 14 a framebuffer readback mismatch,
15 a PSRAM command timeout, 16 a Flash read/range error, 17 an asset
header mismatch, 18 an image CRC mismatch, and 19–22 microSD init/read/CRC16/write
errors. Cached `L1-DCache` PSRAM mappings and Flash XIP remain separate work.


### ext4 system partition

Vela mounts one primary Linux partition (MBR type `0x83`) on demand in the
`ext4fs` U-mode service. Prepare a dedicated partition on Linux. Keep it outside
the legacy artwork and benchmark ranges if you need both (starting the partition
at 1 GiB leaves room for both). Legacy writes are rejected if they overlap an
MBR partition. GPT and partitionless ext4 are not mountable in this release.

Format **the intended partition** with:

```sh
mkfs.ext4 -b 4096 -O ^metadata_csum,^metadata_csum_seed,^orphan_file /dev/SD_PARTITION
```

Build external utilities with `./scripts/build-tools.sh` and copy all ELF
files from `target/sd/bin/` (`shell.elf`, `cat.elf`, `ls.elf`, `uptime.elf`,
`sdexport.elf`) into the card's `bin/` directory, then unmount cleanly before
inserting the card:

```console
ls /sd/bin
cat /sd/hello.txt
run /sd/bin/uptime.elf
```

At boot, flash `init` reads the board policy compiled from `boards/tab5/init.conf`
(checking `/sd/etc/init` first if present, falling back to `exec /sd/bin/shell.elf`).
If `/sd/bin/shell.elf` is missing or unreadable, `init` prints an emergency alert
explaining how to prepare the microSD card; otherwise it launches and supervises
the shell, respawning it if it exits.

The ext4 reader supports 1–4 KiB blocks, regular files, extent trees and linear
directories up to 1 MiB. It rejects volumes needing journal recovery,
unsupported feature flags, symlinks and indexed directories. Vela supports file creation, writes, directory creation and removal on the mounted ext4 partition. File IPC supports offset-based inline reads (up to 95 payload bytes) and bulk transfers, including ELF loading from microSD. `SPAWN_IMAGE` / `SPAWN_IMAGE_WAIT` can pass a bounded
argument and up to 4 explicit capability handles (each attenuated to its
declared rights mask) so child utilities have zero ambient privileges.

`uptime.elf` reads kernel statistics. `cat.elf` and `ls.elf` speak the unified
File IPC protocol across `/proc`, `/sys`, `/dev` (`sysd`) and `/sd` (`ext4fs`).
`shell.elf` provides the interactive console, dispatches `ls`/`cat`/`uptime`/`sdexport`/`run`,
and supports `echo <val> > <path>` writes to `/sys/*` and `/dev/*`.

### USB-C SD export

Run `sdexport 60` for read-only access, or `sdexport rw` to format the card
or update files from Linux. The whole card appears as a removable disk. Verify
the model and capacity with `lsblk` before partitioning or formatting.

For writable export, unmount every partition on Linux and use the desktop's
Eject action or `sudo eject -s /dev/sdX` on the **whole Tab5 disk**. Vela
acknowledges eject after card writes have completed. It reports an empty LUN
while the host finishes probing, then switches back to the USB console and
reopens the ext4 partition on the next file request.
Writable export groups sectors into up to 128 KiB in PSRAM. SDMMC sends a batch
as one CMD25 through its 32 KiB DMA ring (the last short batch is also committed
before SCSI success). USB-C still operates at Full Speed.
Writable export has no timeout. After an unexpected USB disconnect, reconnect
and eject the disk before trying to access it from Vela.

The Tab5 screen shows the export mode and how to eject. It returns to the
framebuffer console when Vela takes the card back.

USB-C switches from Serial/JTAG to Full-Speed USB OTG during export. Reopen
the console when it reconnects. The `ttyACM` number can change, so use the
Tab5's symlink under `/dev/serial/by-id`.

ELF initial stacks reserve a double-size aligned allocation. PMP exposes the upper half and leaves the lower half inaccessible as a guard. PSRAM programs normally use a 64 KiB PSRAM allocation with a 32 KiB usable stack. SRAM programs retain 4 KiB usable stacks with a 4 KiB guard. THREAD_CREATE accepts THREAD_STACK_GUARD in a5 along with affinity. Pass the full private MEM_ALLOC allocation, twice the desired usable stack size. The kernel closes its lower half, restricts the memory capability to CONTROL, and retains the original allocation for MEM_FREE after thread exit. The four init supervisor threads use this mode with 64 KiB PSRAM allocations (32 KiB usable and 32 KiB guard each), preserving SRAM for the framebuffer console and IRQ services. Each HP kernel stack has a 256-byte boundary marker checked at trap entry and return, with SP validation before a kernel-origin trap frame is stored. Failed checks switch to an independent emergency stack for a fatal hart/SP/PC report. Detection occurs at trap boundaries, so a kernel write beyond the stack can damage memory before detection.


### Bulk ext4 reads and profiling

ext4fs resolves file offsets and extents in userspace. For aligned data,
the kernel checks SD READ authority, the live server-bound RPC token,
partition bounds, the writable bulk grant and the client's current mappings.
SD reads into private DMA SRAM; the kernel copies exactly the permitted
file bytes into the client. Each SD operation is limited to 4 KiB.
Unaligned leading bytes and sparse/unwritten regions use the 64-byte-aligned
staging buffer. The final sector may be read in full, with only the file
tail copied to the client. Interprocess copies preserve memmove semantics.

Build utilities with `./scripts/build-tools.sh`, then copy
`target/sd/bin/readbench.elf` into the SD card's `bin/` directory through
USB export and eject cleanly. The benchmark uses this syntax:

```text
readbench [verify] [single|staged] [4k|64k] /sd/<file>
```

The default is a direct 4 KiB read with `hash=off`.
`64k` selects the client RPC size. `single` selects sector-at-a-time
staging and `staged` selects the batched staging control.
`verify` enables FNV-1a and guard checks, beginning with a request one byte
short of the chosen size to exercise unaligned offsets. It samples IRQ
history every 4 KiB of hashing. The previous `io` prefix is an alias for
a read without hashing.

```console
run /sd/bin/readbench.elf 4k /sd/bin/readbench-data.bin
run /sd/bin/readbench.elf 64k /sd/bin/readbench-data.bin
run /sd/bin/readbench.elf verify 64k /sd/bin/readbench-data.bin
```

The 1 MiB + 123 byte test file was installed separately on the Tab5 card.
At 180 MHz with the display active, two runs per size gave:

| Client request | Elapsed time | Client RPCs including EOF | SD commands |
| --- | --- | --- | --- |
| 4 KiB | 1.366–1.375 s | 258 | 265 |
| 64 KiB | 0.628–0.629 s | 18 | 265 |

The 64 KiB mode reached about 1.59 MiB/s, with a maximum observed timer
latency of 1.34 ms. Wide verification matched the Linux FNV-1a value
`2740238281` and passed buffer-tail checks.

These measurements include instrumentation. Detailed ext4 and namespace
timings currently run for ordinary bulk reads too. Reported service times
are nested; kernel counters are cumulative and the utility reports deltas.
Making profiling opt-in is the next experiment. Full profiles, historical
comparisons, hardware observations and limits are in
[the roadmap](docs/roadmap.md).

Keyboard code and its guarded stack use PSRAM to reserve SRAM for fbcon/DMA.
The standard check includes host regressions for copy alignment, tails,
overlapping shared mappings, permissions and destination bounds.
