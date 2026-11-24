# Roadmap

## Completed (ESP32-P4 v1.3 / M5Stack Tab5)

1. **Two-stage boot & memory protection**: SPM `.entry` loader with CRC32-bound upper SRAM payload, L2 reclaimed as SRAM, L1-ICache0/1 enabled, L1-DCache bypassed, deny-by-default NAPOT PMP (`AddressSpace` + cached PMP images).
2. **Syscalls & U-mode isolation**: SPM-resident CLIC trap entry/exit, validated user buffers (`copy_from_user`/`copy_to_user_within`), zero ambient PID authority.
3. **Processes, ELF/PIE & memory pools**: static PIE loader (`R_RISCV_RELATIVE`) in managed internal SRAM (4 KiB NAPOT) and 32 MiB external PSRAM (64 KiB MMU), plus runtime `/sd/bin/*.elf` spawning (`SPAWN_IMAGE` / `SPAWN_IMAGE_WAIT`) with multi-capability handoff (up to 4 attenuated handles).
4. **Preemptive SMP scheduler & extended context**: 100 Hz priority round-robin with transitive priority inheritance, dual-core HP0+HP1 SMP with IPI rescheduling and synchronous PMP shootdowns, per-thread RV32F FPU + ESP32-P4 PIE (`Xespv2p1`) context save/restore, DVFS (`40 <-> 180 MHz`), and tickless idle.
5. **IPC & capabilities**: generational capability tables, bounded FIFO endpoints, synchronous `IPC_CALL`/`IPC_REPLY`/`IPC_REPLY_RECV` with direct handoff, one-level `CAP_GRANT`/`MEM_GRANT`, and clean revocation/teardown in `vela-kernel::lifecycle`.
6. **LP Sentinel**: standalone RV32IMAC supervision firmware in LP SRAM (`0x50108000`) with heartbeat monitoring, diagnostic journal, and coordinated wake/sleep timers over the hardware mailbox.
7. **Userspace services & unified File IPC**:
   - `sysd` serving `/proc`, `/sys`, and `/dev` (`INA226` power, DVFS, display, Sentinel, I²C bus scans, `kbdled`, TRNG/CSPRNG);
   - `ext4fs` mounting MBR ext4 (`/sd`) over the 4-bit SDMMC IDMAC host driver;
   - `console` (USB Serial/JTAG IRQ), `kbd` (Tab5 Keyboard over Ext.Port1 I²C + `GPIO50` IRQ), `fbcon` (PSRAM framebuffer console over MIPI-DSI/DW-GDMA with ANSI SGR and idle blanking);
   - board-configured flash `init` (`boards/tab5/init.conf`) launching and supervising `/sd/bin/shell.elf`, plus standalone `/sd/bin/{cat,ls,uptime,sdexport}.elf`.

## Next

1. **Writable ext4 & dynamic `/dev` registration**: file writes/creation on `/sd` and runtime driver registration into `sysd` (e.g. `/dev/fb*` and loadable device services).
2. **Display wallpapers & graphics**: reuse the retained `vela_boot_and_display_assets/` pipeline for boot/background wallpapers alongside `fbcon`.
3. **Cache & storage performance**: evaluate cached `L1-DCache` mappings with explicit DMA cache maintenance and Flash XIP for larger images.
4. **Portability**: board configs (`boards/<board>/init.conf`) and independently verified SoC boot/PAC/BSP backends for smaller RISC-V targets without SD cards (ESP32-C3, C5, C6, S31).
