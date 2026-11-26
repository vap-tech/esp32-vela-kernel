# Roadmap

## Фундаментальные приоритеты ОС (взгляд без привязки к старому плану)

### 1. Потолок процессов и размера программ (главное узкое место прямо сейчас)
Ядро поддерживает 16 процессов и 16 потоков. `init` и shell загружают с SD статические PIE ELF до 1 MiB с использованием bulk IPC.

### 2. Быстрый транспорт поверх IPC (shared memory вместо 95 байт на вызов)
Сейчас весь File IPC гоняет по **95 байт за один `IPC_CALL`**. Для `/proc/uptime` это идеально, но для:
- отрисовки графики/окон в `/dev/fb0` (~1.8 MiB на кадр),
- загрузки больших ELF с SD,
- аудио, сети, копирования файлов

95-байтные сообщения станут бутылочным горлышком.
В ядре уже есть `MEM_ALLOC` + `MEM_GRANT` — не хватает стандартного протокола **bulk IPC / ring-buffer в общей памяти**, чтобы управляющие команды шли короткими RPC, а данные — zero-copy через переданный регион памяти.

### 3. Динамическая регистрация сервисов в пространстве имён (`/dev`, `/mnt`)
Сейчас «всё есть файл» зашито статически в `src/platform/src/boot.rs`: при старте `ext4fs` жёстко подключён к `sysd`.
Настоящая сила микроядра (как в Plan 9 / QNX / Fuchsia) появляется, когда **любой запущенный позже драйвер** может сказать `sysd`: *«вот мой endpoint, смонтируй меня в `/dev/fb1` или `/dev/audio`»* (естественно, по capability, а не кому попало).
Тогда драйверы действительно выносятся на SD и подключаются на лету без перепрошивки флеша.

### 4. Живучесть микроядра (реальный супервизор и восстановление драйверов)
Главный смысл микроядра — **падение драйвера не роняет ОС**.
`init` перезапускает shell и драйверы `kbd`, `fbcon`, `ext4fs`. После выхода `ext4fs` новый экземпляр регистрирует свой endpoint в работающем `sysd` по пути `/sd`. При сбое `sysd` ядро сохраняет endpoint пространства имён. Новый `sysd` подключается к нему и к живому `ext4fs`. После перезапуска `sysd` восстанавливает и динамические монтирования живых драйверов.

Что делает микроядро по-настоящему живучим:
- `init` держит серверные endpoint'ы или отслеживает смерть **любого** системного сервиса и автоматически его перезапускает;
- клиенты умеют пережить потерю канала (перезапросить путь через `sysd` или получить восстановленный endpoint).

### 5. Нормальный ввод-вывод (`stdin`/`stdout`/`stderr`) и запись на диск
Две вещи, которые отделяют «прошивку с консолью» от «ОС»:
- **Унифицированные потоки ввода-вывода**: сейчас `sys::write` бьёт напрямую в ядерный лог/консоль, а `stdin` есть только у `shell`. Если передавать процессам capability на `stdin`/`stdout` (как обычные файловые/IPC-хендлы), сразу без костылей появятся интерактивные приложения (редактор, меню, TUI), пайпы (`ls | ...`), перенаправление вывода в файл и несколько виртуальных терминалов/окон на экране.
- **Персистентность (запись на носитель)**: ОС без записи остаётся read-only стендом. Возможность сохранять конфиги (`/sd/etc/init`, настройки подсветки/сети), логи и файлы пользователей — ключевой шаг (будь то запись в ext4 или отдельный надёжный RW-раздел вроде LittleFS/FAT).

### 6. Инженерная гигиена ядра и границы для приложений
- **Защита от переполнения стека** (guard-зоны для ядерных и пользовательских стеков) и понятная диагностика падений юзерских задач (понятный crash-report: какой процесс, на каком адресе, почему — вместо молчаливого выхода).
- **Чистый userland SDK**: отделить «библиотеку приложений Vela» (`open`/`read`/`write`/`spawn`/alloc) от внутренностей ядра, чтобы писать программы под Vela было так же просто, как консольные утилиты под Linux.
- **Связность (сеть / Wi-Fi / BLE / USB)** — если думать про реальное использование Tab5 как карманного компьютера: как только у ОС есть файловая система с записью, ввод-вывод, живые приложения и сеть (через ESP32-C6 на Tab5 или USB), устройство перестаёт быть «демо-стендом» и на нём реально можно что-то делать.

### Приоритет на ближайшие шаги (максимум эффекта на единицу усилий)
1. **Shared-memory bulk IPC** для загрузки ELF и других крупных передач.
2. **Динамический `mount` в `sysd`** (чтобы драйверы реально могли жить на SD и появляться в `/dev`).
3. **Унифицированные `stdin`/`stdout` и запись на SD**.

После этих трёх пунктов архитектура перестаёт ограничивать вообще — поверх неё можно строить хоть TUI/GUI-оболочку, хоть сеть, хоть перенос на C3/C6 без переделки фундамента.


> **Статус первых трёх приоритетов — выполнено:**
> 1. **Лимиты + PSRAM ELF + bulk IPC** (`eba8039`): пулы расширены до `16` процессов/задач/endpoint'ов, `8` PMP-регионов и `48` аллокаций; загрузка ELF в PSRAM до `1 MiB`; shared-memory bulk IPC (`IPC_CALL_BULK`/`IPC_BULK_IO`/`IPC_BULK_FORWARD`).
> 2. **Динамический `mount` в `sysd`** (`ec1c503`): `IPC_CALL_CAP`/`IPC_ACCEPT_CAP` + `CAP_ADMIN` для `mount`/`umount` на лету, PIE NAPOT-тайлинг, перенос `ext4fs`/`fbcon` в PSRAM и подгружаемый драйвер `/sd/bin/ramdev.elf`.
> 3. **Унифицированные `stdin`/`stdout` и запись на SD** (`6723bcb` + `rm`/`mkdir`): потоковые endpoint'ы (`ENDPOINT_STREAM` с авто-EOF), `vela_user::io`, пайпы `|` и редирект `>` в `shell`, `/sd/bin/wc.elf`, а также запись, создание и удаление файлов/каталогов в ext4 на `/sd` (`write`, `mkdir`, `rm`).

---

## Completed (ESP32-P4 v1.3 / M5Stack Tab5)

1. **Two-stage boot & memory protection**: SPM `.entry` loader with CRC32-bound upper SRAM payload, L2 reclaimed as SRAM, L1-ICache0/1 enabled, L1-DCache bypassed, deny-by-default NAPOT PMP (`AddressSpace` + cached PMP images, 8 regions per process) with PIE NAPOT tiling.
2. **Syscalls & U-mode isolation**: SPM-resident CLIC trap entry/exit, validated user buffers (`copy_from_user`/`copy_to_user_within`), zero ambient PID authority.
3. **Processes, ELF/PIE & memory pools**: 16 processes/tasks/endpoints and 48 allocations; static PIE loader (`R_RISCV_RELATIVE`) in managed internal SRAM (4 KiB NAPOT) and 32 MiB external PSRAM (64 KiB MMU, runtime ELFs up to 1 MiB); runtime `/sd/bin/*.elf` spawning (`SPAWN_IMAGE` / `SPAWN_IMAGE_WAIT`) with multi-capability handoff (up to 4 attenuated handles).
4. **Preemptive SMP scheduler & extended context**: 100 Hz priority round-robin with transitive priority inheritance, dual-core HP0+HP1 SMP with IPI rescheduling and synchronous PMP shootdowns, per-thread RV32F FPU + ESP32-P4 PIE (`Xespv2p1`) context save/restore, DVFS (`40 <-> 180 MHz`), and tickless idle.
5. **IPC, bulk transfer & capabilities**: generational capability tables, bounded FIFO and stream (`ENDPOINT_STREAM` with automatic 0-byte EOF) endpoints, synchronous `IPC_CALL`/`IPC_REPLY`/`IPC_REPLY_RECV` with direct handoff, capability delegation (`IPC_CALL_CAP`/`IPC_ACCEPT_CAP`/`CAP_GRANT`/`CAP_ADMIN`), shared-memory bulk IPC (`IPC_CALL_BULK`/`IPC_BULK_IO`/`IPC_BULK_FORWARD`), `MEM_GRANT`, and clean revocation/teardown in `vela-kernel::lifecycle`.
6. **LP Sentinel**: standalone RV32IMAC supervision firmware in LP SRAM (`0x50108000`) with heartbeat monitoring, diagnostic journal, and coordinated wake/sleep timers over the hardware mailbox.
7. **Userspace services, writable ext4 & unified I/O**:
   - `sysd` serving `/proc`, `/sys`, `/dev` (`INA226` power, DVFS, display, Sentinel, I²C bus scans, `kbdled`, TRNG/CSPRNG) plus capability-gated dynamic `mount`/`umount` for loadable services (e.g. `/sd/bin/ramdev.elf`);
   - `ext4fs` (PSRAM) mounting MBR ext4 (`/sd`) read/write (`open`/`read`/`write`/`mkdir`/`unlink`) over the 4-bit SDMMC IDMAC host driver;
   - `console` (USB Serial/JTAG IRQ), `kbd` (Tab5 Keyboard over Ext.Port1 I²C + `GPIO50` IRQ), `fbcon` (PSRAM framebuffer console over MIPI-DSI/DW-GDMA with ANSI SGR and idle blanking);
   - unified capability-based `stdin`/`stdout` (`vela_user::io`) with pipes (`|`) and redirection (`>`) in `/sd/bin/shell.elf`, board-configured flash `init` (`boards/tab5/init.conf`), and standalone `/sd/bin/{cat,ls,uptime,wc,sdexport,ramdev}.elf`.

## Next

1. **Service supervision & driver recovery**: `init` now restarts the bundled `fbcon` and `kbd` after exit. Each replacement keyboard instance binds GPIO50 and sends to the existing terminal endpoint. `init` also restarts `ext4fs`, whose new endpoint replaces the reserved `/sd` mount. A replacement `sysd` serves existing namespace handles and reconnects to the live SD service. Dynamic mounts of surviving drivers are restored with fresh send handles; unmounts and capability revocations discard their recovery entries.
2. **Crash diagnostics, stack guards & userland SDK**: ELF initial stacks now have PMP-denied lower guard regions; U-mode fault reports include PID/TID, decoded cause, PC, fault address and stack bounds, retained in /proc/last_fault. THREAD_CREATE supports opt-in guarded stacks, used by init supervisor threads. Remaining work includes kernel stack guards and a clean application SDK.
3. **Display wallpapers & graphics**: reuse the retained `vela_boot_and_display_assets/` pipeline for boot/background wallpapers alongside `fbcon`.
4. **Cache, storage performance & connectivity**: evaluate cached `L1-DCache` mappings with explicit DMA cache maintenance, Flash XIP for larger images, and networking (ESP32-C6 co-processor or USB).
5. **Portability**: board configs (`boards/<board>/init.conf`) and independently verified SoC boot/PAC/BSP backends for smaller RISC-V targets without SD cards (ESP32-C3, C5, C6, S31).
