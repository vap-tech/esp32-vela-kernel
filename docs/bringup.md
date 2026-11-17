# First Light validation — 2026-11-17

Device: user's M5Stack Tab5, USB Serial/JTAG /dev/ttyACM0.
espflash 4.7.0 board-info: ESP32-P4 v1.3, XTAL 40 MHz, flash 16 MB,
MAC e8:f6:0a:e2:fd:50, Secure Boot disabled, Flash Encryption disabled.
PSRAM capacity and display controller are user-supplied facts, not probed here.
The banner labels board and silicon configuration; mhartid is read at runtime.

## Audit and implementation

Original Git repository contained only untracked Cargo library template with
add() sample/test, three empty architecture files and empty directories.
No substantive Vela implementation or commits were replaced. Removed root src,
empty context/PMP stubs and wrong devkit directory. Created workspace manifests,
boot entry/linker/main/build script, kernel identity, PAC/SoC/architecture/BSP
modules, pinned toolchain, image tools/tests, build/check/flash/monitor scripts,
and documentation. Old reference repository was only read; no files changed there.

## Commands and results

Executed cargo fmt --all, cargo fmt --all -- --check, cargo check --workspace
--locked, cargo build --release --locked, cargo tree --workspace,
python3 -m unittest discover -s tools -p 'test_*.py', llvm-readelf -S -s,
llvm-objdump -d --section=.entry, scripts/check.sh and scripts/flash.sh /dev/ttyACM0.

All final checks passed, without compiler warnings. Five Python test methods
exercise ROM integrity, final-image integrity, truncation, range violations,
binding and malformed payloads. ELF checks include undefined symbols, entry/BSS/
stack bounds and alignments. Tests do not prove hardware behavior.

Linked entry: 920 bytes (8 KiB limit), payload: 3792 bytes, current BSS: 0 bytes.
BSS clearing exists but a nonempty BSS has not been exercised on hardware.
Runtime stack: 80 KiB; boot stack: 8 KiB. No external dependencies or std linkage.
Firmware length: 257808 bytes, CRC32 payload 5a7e9db5.
SHA256 firmware: ffa32ce1a2872b816c0628012b329b436f4d6e864729a0c9db837d9646deb545.

Flash command reported successful write. Subsequent readback at 0x2000 for
257808 bytes was compared byte-for-byte with the final firmware artifact.
Only this image region was written. No eFuse, security changes or full erase.

## Hardware observations

Captured with espflash monitor (Ctrl-R reset), in target/hardware-session.log:

```text
ESP-ROM:esp32p4-eco2-20240710
Build:Jul 10 2024
rst:0x17 (CHIP_USB_UART_RESET),boot:0x30f (SPI_FAST_FLASH_BOOT)
SPI mode:DIO, clock div:1
load:0x30100000,len:0x398
entry 0x30100000
VELA microkernel
Space Flow Project

[boot] ESP32-P4 rev1.3 (build target; host verified)
[board] M5Stack Tab5 (configured)
[arch] riscv32
[core] hart 0 online
[init] First Light
```

Sending ! produced:

```text
[trap] mcause=0x30000003 mepc=0x4ff40542 mtval=0x00000000
```

Ctrl-R then reproduced ROM SPI boot and First Light. The hart and cache mode
readback execute in Rust; mepc belongs to the loaded upper SRAM payload.
This proves ROM -> SPM -> ROM flash read -> verified payload -> Rust -> USB
on this device. Trap diagnostics execute and the fatal handler does not return.
It does not establish correctness of arbitrary traps or corrupted stacks.

## Problems found and resolved

Initial Cargo template was unrelated to requested kernel structure. Early format
check found a generated inline comment swallowing code; corrected before build.
The first ELF checker treated the unnamed null UND symbol as an unresolved symbol;
corrected to reject only named unresolved symbols.

A passive monitor that only cleared DTR/RTS produced no diagnostics after flashing.
The verified workflow uses espflash monitor, which performs its reset sequence.
A late reader can send ? to request banner; no timed heartbeat or unbounded TX waits.
An initial silent monitor did not count as a successful boot.

## Remaining work

First successful flash boot is achieved. Next smallest hardware step: disconnect
and restore power, then capture another SPI boot and First Light. That physical
power-cycle was not performed in this session. Also pending: on-device corrupted
payload/header rejection, nonempty data/BSS probes, stack guard/overflow tests,
long-duration stability and independent reference-register audit. Current startup
inherits ROM USB/clock configuration and uses the reference's cache sequence.
No display/touch/audio/Wi-Fi/SD/PSRAM initialization or scheduler exists.

Raw build artifacts/logs live in ignored target/; this document retains results.
For repeatable checks use scripts/check.sh. For hardware use flash.sh then monitor.sh.
