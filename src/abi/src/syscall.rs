//! Vela RV32 syscall ABI: `a7` = number, `a0..a5` = arguments, `a0` = result
//! (negative errno on failure), `a1` = optional secondary result.
//! No kernel pointers cross the ABI. Numbers are append-only.
pub const WRITE: u32 = 1; // a0=readable user buffer, a1=len (<=128 bytes)
pub const YIELD: u32 = 2;
pub const EXIT: u32 = 3;
pub const TICKS: u32 = 4; // low 32 bits of 100Hz uptime; wraps after ~497 days
pub const STATS: u32 = 5; // a0=writable user buffer, a1=byte len, a2=STATS_* selector
pub const STATS_SUMMARY: u32 = 0; // legacy summary (+ optional USB RX count)
pub const STATS_BYTES: usize = 44; // 4*u32, 2*u64, 3*u32, little-endian
pub const STATS_IRQ_BYTES: usize = STATS_BYTES + 8; // optional USB RX IRQ count at byte 44
pub const STATS_CPU: u32 = 1; // hart, clocks, mcycle breakdown, capacity, IRQ latency, DVFS/tickless
pub const STATS_CPU_BYTES: usize = 72;
pub const STATS_IRQ: u32 = 2; // u64 counts: systimer0, usb_rx, gdma_ahb_ch1, lp_sentinel
pub const STATS_IRQ_ALL_BYTES: usize = 32;
pub const STATS_STREAMS: u32 = 3; // DW-GDMA DSI scanout + GDMA-AHB CH1 counters
pub const STATS_STREAMS_BYTES: usize = 36;
pub const STATS_TASKS: u32 = 4; // 8 slots * 9 u32 words (tid, pid, meta, stack, preempt, ticks, cycles)
pub const STATS_TASK_WORDS: usize = 9;
pub const STATS_TASKS_BYTES: usize = 8 * STATS_TASK_WORDS * 4;
pub const STATS_SDCARD: u32 = 5; // state/flags, ocr, sectors, asset_lba, asset_bytes, read_sectors, written_sectors, errors
pub const STATS_SDCARD_BYTES: usize = 32;
// Userspace-facing ABI values; the kernel itself encodes via `Errno::to_abi`.
pub const EFAULT: u32 = (-14i32) as u32;
pub const EBUSY: u32 = (-16i32) as u32;
pub const EINVAL: u32 = (-22i32) as u32;
pub const ENOSYS: u32 = (-38i32) as u32;
pub const ETIMEDOUT: u32 = (-110i32) as u32;

pub const ENDPOINT_CREATE: u32 = 6;
pub const SEND: u32 = 7; // a0=endpoint handle (SEND), a1=buffer, a2=len; full => EAGAIN
pub const RECV: u32 = 8; // a0=endpoint handle (RECEIVE), a1=buffer, a2=capacity, a3=flags (0 / NONBLOCK)
pub const ENDPOINT_CLOSE: u32 = 9; // a0=endpoint handle
pub const CONSOLE_READ: u32 = 10; // a0=console RX event handle (READ), a1=buffer, a2=capacity (<=32 bytes)
pub const SLEEP: u32 = 11; // a0=relative 100Hz ticks, 1..6000, blocks
pub const IPC_BYTES: usize = 96;
pub const NONBLOCK: u32 = 1;
pub const EAGAIN: u32 = (-11i32) as u32;

pub const THREAD_CREATE: u32 = 12; // a0=entry, a1=stack base, a2=size, a3=arg, a4=priority, a5=affinity
pub const PROCESS_EXIT: u32 = 13;

pub const MEM_ALLOC: u32 = 14; // a0=requested bytes, a1=MEM_CLASS_* -> a0=base, a1=actual size
pub const MEM_FREE: u32 = 15; // a0=base, a1=exact allocated size
pub const MEM_CLASS_SRAM: u32 = 0; // internal SRAM pool (4 KiB granularity)
pub const MEM_CLASS_PSRAM: u32 = 1; // external 32 MiB AP HEX PSRAM pool (64 KiB MMU page granularity)

pub const CAP_GRANT: u32 = 16; // a0=source handle (GRANT), a1=target PID, a2=subset rights -> peer handle
pub const CAP_REVOKE: u32 = 17; // a0=owner handle (CONTROL) -> revokes all peer grants
pub const MEM_CAP: u32 = 18; // a0=owned region base -> memory capability handle
pub const MEM_GRANT: u32 = 19; // a0=memory handle (GRANT), a1=target PID, a2=R/RW -> peer handle + PMP mapping

pub const IPC_CALL: u32 = 20; // a0=endpoint handle (SEND), a1=req, a2=len, a3=reply buf, a4=cap, a5=timeout ticks
pub const IPC_REPLY: u32 = 21; // a0=single-use reply token, a1=buffer, a2=len
pub const THREAD_PRIORITY: u32 = 22; // a0=own TID, a1=priority 0..ceiling
pub const SPAWN_HELLO: u32 = 23; // a0=ProcessSpawn handle (EXECUTE), a1=selector (0=hello, 1=pic, 2=dma, 3=pic-psram) -> PID
pub const EVENT_WAIT: u32 = 24; // a0=event handle (WAIT | ACK), a1=timeout ticks (0=infinite, 1..=6000)
pub const DMA_COPY: u32 = 25; // a0=DMA channel capability handle (READ|WRITE|WAIT) or legacy len (a1=0), a1=len (4..=2048); block until IRQ
pub const SENTINEL_STATUS: u32 = 26; // a0=Sentinel handle (READ) -> a0=version (0 until READY), a1=epoch
pub const SENTINEL_HELLO: u32 = 27; // a0=Sentinel handle (READ); blocks for ACK, returns sequence
pub const SENTINEL_QUERY: u32 = 28; // a0=Sentinel handle (READ), a1=writable 24B buffer, a2=size
pub const SENTINEL_CONTROL: u32 = 29; // a0=Sentinel handle (CONTROL), a1=0 pause / 1 resume HP heartbeat
pub const SENTINEL_DIAG: u32 = 30; // a0=Sentinel handle (WRITE), a1=code, a2=value; blocking ACK
pub const SENTINEL_LOG: u32 = 31; // a0=Sentinel handle (READ), a1=seq (0 metadata), a2=writable 24B, a3=size
// Metadata: epoch,oldest,newest,overwritten,capacity,clock_hz; a1=received diag notices.
// Record: sequence,type,timestamp_lo,timestamp_hi16,arg0,arg1 (LP ticks since LP startup).

pub const SENTINEL_ARM_WAKE: u32 = 32; // a0=Sentinel handle (WAIT), a1=1..60000ms -> wake token
pub const SENTINEL_WAIT_WAKE: u32 = 33; // a0=Sentinel handle (WAIT), a1=token (0=current) -> a0=token, a1=elapsed ticks
pub const SENTINEL_CANCEL_WAKE: u32 = 34; // a0=Sentinel handle (WAIT), a1=token (0=current) -> ACK or EALREADY
pub const SENTINEL_PREPARE_SLEEP: u32 = 35; // a0=Sentinel handle (WAIT | CONTROL), a1=ms -> wake token
pub const DISPLAY_TEST: u32 = 36; // a0=Display handle (CONTROL), a1=DISPLAY_MODE_* -> delay ticks / 0 / -errno
pub const DISPLAY_MODE_STEP: u32 = 0; // advance in-flight display bring-up stage
pub const DISPLAY_MODE_BARS: u32 = 1; // ST7121 MIPI-DSI hardware color bars
pub const DISPLAY_MODE_ORBIT: u32 = 2; // DW-GDMA CH0 64-row Orbit band scanout
pub const DISPLAY_MODE_PIXEL: u32 = 3; // DW-GDMA CH0 single center pixel scanout
pub const DISPLAY_MODE_DIAG: u32 = 4; // Full-screen 1280x720 PSRAM diagnostic artwork
pub const DISPLAY_MODE_BOOT: u32 = 5; // Full-screen 1280x720 PSRAM boot artwork
pub const DISPLAY_MODE_SDCARD_PROBE: u32 = 6; // Verify/provision microSD VELAIMG1 asset bundle
pub const DISPLAY_MODE_OFF: u32 = 7; // Stop DSI/DW-GDMA scanout, disable backlight, release 2 MiB PSRAM framebuffer
pub const IPC_REPLY_RECV: u32 = 37; // a0=reply token (0=skip), a1=reply buf, a2=reply len, a3=recv handle, a4=recv buf, a5=recv cap -> a0=len, a1=next token
pub const IRQ_ACK: u32 = 38; // a0=event handle (ACK); clear pending notification and re-arm hardware IRQ source
pub const IRQ_BIND: u32 = 39; // a0=IrqControl handle (CONTROL), a1=irq slot index -> bound event handle
pub const IRQ_UNBIND: u32 = 40; // a0=bound event handle (CONTROL); mask line, wake waiters with -EPIPE, release slot
pub const I2C_TRANSFER: u32 = 41; // a0=I2c handle (READ|WRITE), a1=7-bit addr, a2=tx_ptr, a3=tx_len (<=16), a4=rx_ptr, a5=rx_len (<=16) -> 0 / -errno
pub const I2C_MAX_BYTES: usize = 16;
pub const CLOCK_CONTROL: u32 = 42; // a0=Sentinel handle (CONTROL), a1=FREQ_MODE_* -> a0=active cpu_mhz, a1=active mode
pub const FREQ_MODE_AUTO: u32 = 0; // DVFS idle governor: 180 MHz active, 40 MHz in tickless/prepared sleep
pub const FREQ_MODE_40MHZ: u32 = 1; // Pin 40 MHz XTAL (rejected with -EBUSY while display/DMA active)
pub const FREQ_MODE_180MHZ: u32 = 2; // Pin 180 MHz CPLL
pub const FREQ_MODE_QUERY: u32 = 3; // Query current cpu_mhz and FREQ_MODE_* without changing policy

pub const IRQ_SLOT_CONSOLE_RX: u32 = 0; // USB Serial/JTAG RX packet interrupt slot (CLIC line 18)

pub const DMA_CHANNEL_GDMA_AHB_CH1: u32 = 0; // GDMA-AHB channel 1 memory-to-memory copy
pub const DMA_CHANNEL_DW_GDMA_SCANOUT: u32 = 1; // DW-GDMA channel 0 MIPI-DSI video scanout
