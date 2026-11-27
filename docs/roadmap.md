# Roadmap

## Ближайший эксперимент — пакетное чтение ext4 на Tab5

Начать с пути приложения: `OP_READ_BULK` → `ext4fs` → `Ext4::read` → `BlockRead::read_sector` → `SD_PARTITION`. Запрос приложения уже передаётся через bulk IPC. На границе с ядром драйвер запрашивает по одному сектору (512 байт). Для 4 КиБ выровненных последовательных данных это восемь вызовов. Точки входа — [ext4fs.rs](../src/user/src/bin/ext4fs.rs) и [ext4.rs](../src/user/src/ext4.rs).

1. Снять базовый замер на существующем файле microSD. Зафиксировать размер, частоту CPU и состояние дисплея. Измерить время чтения без вывода содержимого на экран, количество команд чтения SD и задержку системного тика. Счётчик прочитанных секторов и число команд учитывать отдельно.
2. Добавить чтение до восьми последовательных секторов за команду для выровненной части файла. Ограничивать пакет границей непрерывного участка и раздела. Одиночный путь использовать для невыровненного начала, хвоста и устройств без пакетного чтения.
3. Повторить на том же файле в тех же условиях. Сверить данные побайтно, включая неполный последний сектор, EOF и чтение через границу extent. Записать разницу во времени, количестве команд и задержке тика. Эффект на плате пока не измерен.

**Что уже исправлено в рабочем дереве.** Пустой опрос журнала `fbcon` больше не запускает обход 80×22 ячеек. При периоде 20 мс это устраняет до 88 000 сравнений в секунду. Выигрыш в процентах CPU отдельно не измерен. В `CycleSnapshot::utilization_bps` исправлено переполнение при долгой работе и добавлен тест для границы `u64`. Показатель `/proc/cpuinfo` вычисляет своя функция `ratio_bps`. К планировщику вернуться, если профиль покажет заметную стоимость выбора потока или наследования приоритета.

## Исходные архитектурные приоритеты (история проекта)

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

8. **Service supervision & driver recovery**: `init` now restarts the bundled `fbcon` and `kbd` after exit. Each replacement keyboard instance binds GPIO50 and sends to the existing terminal endpoint. `init` also restarts `ext4fs`, whose new endpoint replaces the reserved `/sd` mount. A replacement `sysd` serves existing namespace handles and reconnects to the live SD service. Dynamic mounts of surviving drivers are restored with fresh send handles; unmounts and capability revocations discard their recovery entries.
9. **Crash diagnostics, stack guards & userland SDK**: ELF initial stacks now have PMP-denied lower guard regions; U-mode fault reports include PID/TID, decoded cause, PC, fault address and stack bounds, retained in /proc/last_fault. THREAD_CREATE supports opt-in guarded stacks, used by init supervisor threads. HP kernel stacks have software boundary markers, pre-save SP checks and separate emergency stacks. M-mode overflow detection happens at trap boundaries. A minimal application SDK now lives in `src/sdk`, with an independent example workspace, path/cursor file API, stdin/stdout, owned PSRAM buffers and explicit-authority child launching. See `docs/sdk.md`. General child stream wiring, richer errors and independently versioned ABI compatibility remain future extensions.

## Next

Сначала выполнить эксперимент с ext4 в начале документа. Ниже — последующие направления.

1. **Display wallpapers & graphics**: keep the small `fbcon` image in SRAM and its framebuffer in PSRAM (618-cell startup render measured at 80 ms versus 2.54 s with the image in PSRAM). Supervisor stacks use guarded PSRAM allocations to preserve internal SRAM. Reuse the retained `vela_boot_and_display_assets/` pipeline for boot/background wallpapers alongside `fbcon`.
2. **Cache, storage performance & connectivity**: evaluate cached `L1-DCache` mappings with explicit DMA cache maintenance, Flash XIP for larger images, and networking (ESP32-C6 co-processor or USB).
3. **Portability**: board configs (`boards/<board>/init.conf`) and independently verified SoC boot/PAC/BSP backends for smaller RISC-V targets without SD cards (ESP32-C3, C5, C6, S31).

## Optional — stronger kernel stack guarantees

- **Hardware prevention for M-mode stack overflows.** Investigate an appropriate
  PMP or SoC protection scheme for ESP32-P4 rev1.3 separately. Existing unlocked
  PMP mappings isolate U-mode and do not guard ordinary M-mode writes. Verify
  semantics, entry budget, permissions needed by both HP harts, DMA interaction
  and reset/recovery before adopting locked PMP or any alternative. No hardware
  prevention is claimed by the current marker checks.
- **Whole-call-chain stack bounds.** Extend the current 24 KiB per-frame CFI build
  gate with call-chain analysis where practical. Handle indirect calls, recursion,
  handwritten assembly and emergency reporting explicitly. Record representative
  `/proc/kernel_stacks` measurements after changes to loaders, IPC and drivers.
  The current write watermark is an estimate and does not establish a worst-case
  bound. Details and interpretation are in `docs/memory-map.md`.

## Архив локального TODO от 21 ноября 2026

Ниже сохранены все рабочие записи на указанную дату. Текущий порядок работ приведён в начале roadmap. С тех пор бюджет пользовательских PMP-регионов увеличен до восьми, а замеры прокрутки опубликованы в README. Для проверки чтения ext4 на рабочей карте используем существующий файл; старый порог LBA сам по себе не гарантирует свободное место.

### Vela — оставшаяся работа

Локальный рабочий файл. Не добавлять в Git и не удалять.

Актуализировано 2026-11-21 (клавиатура по прерыванию, цвета консоли). Реализованные механизмы, команды, размеры памяти и архитектура описаны в `README.md`, `docs/architecture.md` и `docs/memory-map.md`.

#### Общие указания

- Продолжай существующее микроядро Vela на Rust для RISC-V. Сведения о реализованных механизмах и командах см. в `README.md`.
- Пиши работающий код небольшими самостоятельными этапами. После каждого законченного этапа делай локальный коммит и переходи к следующему. Независимые этапы можно переставлять, если аппаратная зависимость блокирует работу.
- Проверяй состояние рабочего дерева и включай только относящиеся к этапу файлы. Запрещены новые remote, отправка кода наружу, переписывание истории, удаление чужих веток и коммитов, безусловный `git reset --hard`.
- Перед коммитом кода выполняй `cargo fmt --all` и проверку штатным скриптом `./scripts/check.sh` (или целевую release-сборку `./scripts/build.sh`). После существенного изменения аппаратного пути — одна прошивка и короткая проверка механизма и shell/procfs на Tab5. Используй штатный образ и `./scripts/flash.sh /dev/ttyACM0`. Не прошивай другие устройства, не меняй eFuse и настройки безопасности.
- Не повторяй полный аудит, power-cycle, readback Flash, CRC injection, полный erase и стресс-тесты ради уверенности. Не переделывай bootloader и packer без конкретной причины.
- Не вводи пустые crates, фиктивные API и неподтверждённые аппаратные возможности. Используй статические пулы там, где они достаточны. Обновляй `README.md` кратко при изменении команд, возможностей или ограничений. При необходимости вынеси инварианты HP/LP и памяти в один небольшой документ.
- В итоговом отчёте (до 12 строк) укажи результат, что проверено на Tab5, ограничения и аппаратные блокеры, локальные коммиты и ближайший этап.

#### Платформа и архитектурные ограничения

Цель — M5Stack Tab5, ESP32-P4 **v1.3**, два HP hart и отдельный LP CPU. XTAL 40 МГц, Flash 16 МиБ, PSRAM 32 МиБ, LP SRAM 32 КиБ, LP ROM 16 КиБ. Консоль — USB Serial/JTAG. Адреса, ROM ABI, частоты и последовательности инициализации сверяй для этой ревизии.

- [TRM ESP32-P4 v1.3](https://documentation.espressif.com/esp32-p4-chip-revision-v1.3_technical_reference_manual_en.pdf)
- [Datasheet ESP32-P4 v1.3](https://documentation.espressif.com/esp32-p4-chip-revision-v1.3_datasheet_en.html)
- [Errata ESP32-P4](https://docs.espressif.com/projects/esp-chip-errata/en/latest/esp32p4/)

Сохраняй разделение arch / SoC / BSP / kernel / ABI / userspace. Общему ядру не нужны адреса регистров P4, GPIO Tab5 или сведения о ST7121. Поддерживай конфигурацию с одним HP для C6. Топологию C5 и S31 уточняй по их документации, когда понадобится порт.

Сохраняй PMP-изоляцию и проверку диапазонов с учётом переполнения. На HP ограничен бюджет в 16 PMP entries (сейчас 6 активных пользовательских слотов `pmp0..5` и 10 занулённых); учитывай гранулярность, TOR/NAPOT и locked entries, не рассчитывай на NA4 или Sv32. Не сохраняй долгоживущие ссылки на пользовательскую память. Доступ к MMIO и дескрипторам устройств контролирует доверенный слой. Ядро пока non-preemptible, переключение контекста — на выходе из trap. `L1-ICache0` и `L1-ICache1` (по 16 КиБ) включены с явной инвалидацией через `soc::invalidate_icache` + `fence.i` при загрузке исполняемых сегментов ELF; `L1-DCache` остаётся в режиме bypass.

#### Оставшаяся работа

##### 1. Дополнительные опции сна, дисплея и памяти (опционально)

- **Глубокий PMU sleep и сохранение журнала LP.** При необходимости реализовать отключение питания домена HP через PMU light/deep-sleep с полным аппаратным рестартом/восстановлением (`PMP`, `CLIC`, `L1-ICache0`/`L1-ICache1`, `PSRAM`), retention-протоколом журнала LP через HP reboot и политикой LP watchdog.
- **Ориентация ввода.** При добавлении сенсорного/графического интерфейса согласовать координаты ввода с принятой landscape-ориентацией (порт клавиатуры снизу).
- **Flash/XIP и кэшируемый `L1-DCache`.** При необходимости предусмотреть Flash/XIP для неизменяемых данных и кода и аппаратный clean/invalidate в `prepare_dma_buffer`/`finish_dma_buffer` при включении `L1-DCache`, учитывая APM-560 и APM-885.

##### 2. microSD: асинхронные передачи по прерыванию IDMAC (приоритет)

Состояние: 4-bit SDMMC (DWC-MSHC `0x5008_3000`), High-Speed 40 МГц, IDMAC-кольцо из 256 дескрипторов (`_sdmmc_idma_chain`, 4 КиБ) и 32 КиБ буфера (`_sdmmc_dma_buf`, 4 сегмента по 8 КиБ) в L2MEM. Замер на Tab5: запись ≈ 11 МиБ/с, чтение ≈ 18,4 МиБ/с (≈ 96 % предела шины 19,07 МиБ/с), все с `verify=ok`. Накладные на команду: чтение ≈ 0,36 мс, запись ≈ 1,3–1,7 мс.

Проблема: `sdmmc_stream` блокирует HP0 на всю команду, прерывания маскируются, а бенч поднимает CPU до 180 МГц. Длина команды определяет задержку системы: 1 МиБ ≈ 91 мс записи / 54 мс чтения, 64 КиБ ≈ 7 / 4 мс (меньше тика 10 мс). Сейчас по умолчанию `sdcard bench` использует 64 КиБ (запись ≈ 9,0, чтение ≈ 16,7 МиБ/с); короткие команды теряют скорость (16 КиБ: 5,1 / 13,0).

Решение: не ужимать команду, а сделать передачу асинхронной. DMA работает, ядро занимается другими задачами и просыпается по прерыванию IDMAC/SDHOST.

- Прерывания: источник SDHOST в interrupt matrix → CLIC-линия на HP0. Использовать `IDSTS`/`IDINTEN` (TI/RI/NIS/AIS/DU/FBE), `RINTSTS`/`INTMASK` (DTO, CD, RTO/RCRC/DCRC/DRTO, SBE, EBE). Очищать биты W1C; учитывать, что `release_stale_ipi_line` в `soc/smp.rs` не должна трогать эту линию.
- Модель драйвера: конечный автомат запроса (Idle → Cmd → Data → Stop → Done/Error) с таймаутом по системному тику вместо busy-wait; одна активная команда, очередь запросов в статическом пуле.
- Кольцо дескрипторов: пополнение/проверка сегментов (сейчас делает CPU в `sdmmc_stream`) переносится в ISR по RI/TI с очисткой OWN; ошибка `ERR_SD_OVERRUN` остаётся для случая, когда ISR не успел. Проверить поведение на CPU 40 МГц (сейчас не проверено).
- Блокировка задач: syscall записи/чтения переводит вызывающую задачу в состояние ожидания на событии устройства; ISR будит её через существующий механизм (по аналогии с `ipc recv blocked`). Остальные задачи и дисплей продолжают работать, LP Sentinel keepalive не должен зависеть от бенча.
- Кэш/когерентность: `L1-DCache` в bypass, но при его включении нужен clean/invalidate буферов (`prepare_dma_buffer`/`finish_dma_buffer`).
- Изоляция: буфер пользователя копируется доверенным слоем (без долгоживущих ссылок на пользовательскую память), диапазон LBA проверяется с учётом переполнения; не трогать LBA 2048..2580 (ресурсы дисплея), бенч пишет только с LBA ≥ 16384.
- Тест: `sdcard bench` с параллельной нагрузкой (`bench`, `uptime`, вывод дисплея); измерить задержку тика и джиттер при команде 1 МиБ (цель — без роста задержки, скорость как у 1 МиБ ≈ 11 / 18,4 МиБ/с), `verify=ok`, ошибок 0 в `/proc/sdcard`.
- SPI-fallback остаётся блокирующим (медленный путь), это зафиксировать в README.

##### 3. microSD: следующие шаги после async

- Минимальная FAT32 только для чтения (малые файлы): сначала `cat`/листинг, затем запись. Для малых файлов важна латентность одной команды (≈ 0,4 мс чтение / ≈ 1,5 мс запись), а не пиковая скорость, поэтому нужны кэш секторов и FAT.
- Низкое энергопотребление: gating тактов SDHOST в простое (`CLKENA.lp_enable`), отключение `LDO_VO4`, когда карта не нужна (питание карты жёстко 3,3 В, программно полностью обесточить нельзя; `CMD0` при CS=low фиксирует SPI-режим до физического отключения питания).
- 50 МГц / UHS-I: не планируется. Требуется выделенный SDIO PLL (`src_sel=1`, 200 МГц) и сигналы 1,8 В; выигрыш мал по сравнению с риском. Сейчас используется общий PLL_160M (как в ESP-IDF).
- Очистка отладочных полей `step`/`rint`/`st`/`clk02` в выводе `[sdcard]` и `/proc/sdcard`.
- Команды `rand` и `/proc/random` (сейчас только `cat /dev/random` — TRNG, и `cat /dev/zero`).

##### 4. Tab5 Keyboard (A164) по Ext.Port1

Подтверждено на Tab5: `i2cdetect 1` находит `0x6d` на шине 1 (`GPIO0` SDA, `GPIO1` SCL; INT = `GPIO50`). Схемы Tab5 и клавиатуры сверены: на плате клавиатуры подтяжки `4.7K` на SDA/SCL и `10K` на INT к `VCC_3V3` (питание идёт от `SOC_3.3V` разъёма J9, поэтому светодиод горит без `EXT5V_EN`). Протокол (STM32F030C8T6) взят из вторичных источников (поиск по M5Tab5-Keyboard-Internal-FW): регистры `0x00` INT_CFG, `0x01` INT_STA, `0x02` EVENT_NUM, `0x10` режим (0 Normal, 1 HID, 2 Character), `0x20` Normal-событие, `0x30` HID (2 байта), `0x40` длина строки, `0x50` строка Character-режима, `0xF0` версия/адрес. PDF `Tab5_Keyboard-I2C-Protocol` с описанием регистров сам не читал: перед драйвером сверить.

- [x] Userspace-драйвер `kbd` на шине 1 (`I2C_TRANSFER`, Character-режим даёт готовый ASCII, включая Aa/Sym/Ctrl/Alt), подключение к вводу shell рядом с USB-консолью. Узла `/dev/kbd` нет, не нужен.
- [x] Прерывание по `GPIO50`: low-level на CLIC-линию 22 (`soc/gpio_irq.rs`), слот `IRQ_SLOT_KBD = 1`, `kbd` блокируется в `EVENT_WAIT` без таймаута и опроса. Прошивка клавиатуры держит INT низким, пока FIFO не пуст, и отпускает его только по записи `INT_STA = 0`; без очистки защёлки статуса GPIO (`STATUS1_W1TC`) линия остаётся pending. Счётчики `kbd_gpio50`, `kbd_arm_low`, `kbd_arm_wait` в `/proc/interrupts`. Подтверждено на железе: одно прерывание на нажатие или пачку.
- [x] Светодиоды: `kbdled` (регистры `0x03`, `0x11`, `0x60`, `0x64`).
- Остаётся открытым: раскладка (кириллица); возможная смена адреса через `0xF0` (в `data_access` шины 1 сейчас разрешён любой адрес); перепроверить протокол по PDF `Tab5_Keyboard-I2C-Protocol` (регистры взяты из прошивки и подтверждены на железе, но сам PDF не читался).

##### 5. Консоль на экране (автономное устройство)

Цель: экран + Tab5 Keyboard вместо USB-терминала. Ландшафт «клавиатура снизу» 1280×720; в PSRAM фреймбуфер 720×1280 RGB565, пиксель ландшафта (u,v) → нативный (X=v, Y=1279-u) (PIL ROTATE_90, tools/display_assets.py). Шрифт Terminus 16×32 (OFL, из /usr/share/consolefonts/CyrAsia-Terminus32x16.psf.gz, tools/gen_font.py), сетка 80×22 (ширина строки shell 80).

Решение: фреймбуфер принадлежит userspace-процессу vela-fbcon (MEM_ALLOC 2 МиБ PSRAM после стадии 2, то есть после захвата CPLL 180 МГц). Ядро только сканирует: DISPLAY_MODE_CONSOLE проходит стадии 0..2, затем ждёт STEP с a2=fb_base (владение проверяется), пропускает загрузку с SD и стартует сканаут. Вывод всех sys::write и логов ядра зеркалится в кольцо 4 КиБ (CONSOLE_LOG, Authority::ConsoleLog); fbcon читает его опросом и рисует.

- [x] ABI: CONSOLE_LOG=44, DISPLAY_MODE_CONSOLE=9, Authority::ConsoleLog
- [x] Ядро: кольцо вывода (+ тесты), sys_console_log, режим CONSOLE в DisplayControl (внешний fb, in-flight на владельца, EBUSY для других режимов, OFF отсоединяет)
- [x] Userspace: fbcon (сетка, прокрутка, перерисовка только изменённых клеток)
- [x] Загрузка: 5-й процесс fbcon (Display CONTROL + ConsoleLog READ)
- [x] Проверено на экране: ориентация, шрифт, ввод с клавиатуры, цвета
- [ ] Измерить: время полной перерисовки и прокрутки (по тикам, rdcycle64 при миграции между ядрами врёт); решить, нужен ли DMA-сдвиг
- [ ] Стек процесса 4 КиБ: большие структуры только в .bss
- [ ] Открыто: fbcon читает лог опросом (20 мс, при погашенном экране 100 мс); можно ждать событие вместо опроса
- [ ] Открыто: display-команды shell конфликтуют с консолью (пока EBUSY); команда для повторного включения; sleep и DVFS блокируются, пока дисплей активен

#### 6. Гашение экрана, светодиоды и цвета консоли
- [x] fbcon гасит подсветку через 5 минут без вывода (`IDLE_TICKS`), будит по новому выводу
- [x] `kbdled`: RGB1 0x60..0x62, RGB2 0x64..0x66, режим 0x11, яркость 0x03 (подтверждено на железе)
- [x] Настоящий wake по GPIO50, `kbd` блокируется на событии (общий IRQ-слот, образец для async SD)
- [x] Светодиоды гаснут через 5 минут без клавиш (яркость 0x03 = 0) и возвращаются на следующей клавише
- [x] Клавиша-будильник не попадает в shell: ядро отдаёт `STATS_DISPLAY` (бит 0 = подсветка выключена), `kbd` пишет в консоль `ESC [ m`, это будит fbcon
- [x] fbcon при погашенном экране опрашивает лог раз в 100 мс
- [x] Цвета: ANSI SGR (16 цветов, bold, reset) в fbcon; промпт зелёный, ошибки красные, `[tag]` в начале строки красится по хешу имени, команды в `help` голубые
- [ ] Светодиоды гасить по реальному состоянию экрана (сейчас по простою клавиш, поэтому гаснут чуть раньше экрана)
- [ ] Цвет в остальных командах (`/proc`, `sdcard`, `smp`): заголовки, статусы ok/fail
- [ ] Новогодняя ёлка: ASCII в консоли, графика на весь экран или гирлянда на светодиодах клавиатуры
