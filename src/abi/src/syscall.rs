//! Vela RV32 syscall ABI: `a7` = number, `a0..a5` = arguments, `a0` = result
//! (negative errno on failure), `a1` = optional secondary result.
//! No kernel pointers cross the ABI. Numbers are append-only.
pub const WRITE: u32 = 1; // a0=readable user buffer, a1=len (<=128 bytes)
pub const YIELD: u32 = 2;
pub const EXIT: u32 = 3;
pub const TICKS: u32 = 4; // low 32 bits of 100Hz uptime; wraps after ~497 days
pub const STATS: u32 = 5; // a0=writable user buffer, a1=STATS_BYTES or STATS_IRQ_BYTES
pub const STATS_BYTES: usize = 44; // 4*u32, 2*u64, 3*u32, little-endian
pub const STATS_IRQ_BYTES: usize = STATS_BYTES + 8; // optional USB RX IRQ count at byte 44
// Userspace-facing ABI values; the kernel itself encodes via `Errno::to_abi`.
pub const EFAULT: u32 = (-14i32) as u32;
pub const EINVAL: u32 = (-22i32) as u32;
pub const ENOSYS: u32 = (-38i32) as u32;

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

pub const MEM_ALLOC: u32 = 14; // a0=requested bytes -> a0=base, a1=actual size
pub const MEM_FREE: u32 = 15; // a0=base, a1=exact allocated size

pub const CAP_GRANT: u32 = 16; // a0=source handle (GRANT), a1=target PID, a2=subset rights -> peer handle
pub const CAP_REVOKE: u32 = 17; // a0=owner handle (CONTROL) -> revokes all peer grants
pub const MEM_CAP: u32 = 18; // a0=owned region base -> memory capability handle
pub const MEM_GRANT: u32 = 19; // a0=memory handle (GRANT), a1=target PID, a2=R/RW -> peer handle + PMP mapping

pub const IPC_CALL: u32 = 20; // a0=endpoint handle (SEND), a1=req, a2=len, a3=reply buf, a4=cap, a5=timeout ticks
pub const IPC_REPLY: u32 = 21; // a0=single-use reply token, a1=buffer, a2=len
pub const THREAD_PRIORITY: u32 = 22; // a0=own TID, a1=priority 0..ceiling
pub const SPAWN_HELLO: u32 = 23; // a0=ProcessSpawn handle (EXECUTE), a1=selector (0=hello, 1=pic, 2=dma) -> PID
pub const EVENT_WAIT: u32 = 24; // a0=event handle (WAIT | ACK); acknowledge, rearm, and block
pub const DMA_COPY: u32 = 25; // owning DMA driver: copy a0 bytes between private page halves, block until IRQ
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
pub const DISPLAY_TEST: u32 = 36; // a0=Display handle (CONTROL), a1=mode (1..5 start, 0 step) -> delay ticks / 0 / -errno
