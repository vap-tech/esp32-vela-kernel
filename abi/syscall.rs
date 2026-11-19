//! Initial Vela RV32 ABI: a7=number, a0..a2=args, a0=result (negative errno).
//! No kernel pointers cross the ABI. Numbers are append-only.
pub const WRITE: u32 = 1; // a0=readable user buffer, a1=len, <=128 bytes
pub const YIELD: u32 = 2;
pub const EXIT: u32 = 3;
pub const TICKS: u32 = 4; // low 32 bits of 100Hz uptime; wrap after ~497 days
pub const STATS: u32 = 5; // a0=writable user buffer; explicit wire layout below
pub const STATS_BYTES: usize = 44; // 4*u32, 2*u64, 3*u32, little endian
pub const STATS_IRQ_BYTES: usize = STATS_BYTES + 8; // optional USB RX IRQ count at byte 44
pub const EFAULT: u32 = (-14i32) as u32;
pub const EINVAL: u32 = (-22i32) as u32;
pub const ENOSYS: u32 = (-38i32) as u32;

pub const ENDPOINT_CREATE: u32 = 6;
pub const SEND: u32 = 7; // a0=local handle, a1=buffer, a2=len; full => EAGAIN
pub const RECV: u32 = 8; // a0=handle,a1=buffer,a2=capacity,a3=flags (0 / NONBLOCK)
pub const ENDPOINT_CLOSE: u32 = 9;
pub const CONSOLE_READ: u32 = 10; // assigned console task only; nonblocking <=32 bytes
pub const SLEEP: u32 = 11; // a0=relative 100Hz ticks, 1..6000, blocks
pub const IPC_BYTES: usize = 96;
pub const NONBLOCK: u32 = 1;
pub const EAGAIN: u32 = (-11i32) as u32;

pub const THREAD_CREATE: u32 = 12; // a0=entry,a1=stack base,a2=size,a3=arg,a4=priority,a5=affinity
pub const PROCESS_EXIT: u32 = 13;

pub const MEM_ALLOC: u32 = 14; // a0=requested bytes -> a0=base,a1=actual size
pub const MEM_FREE: u32 = 15; // a0=base,a1=exact allocated size

pub const CAP_GRANT: u32 = 16; // a0=source handle,a1=target PID,a2=subset rights -> peer-local handle
pub const CAP_REVOKE: u32 = 17; // owner control handle revokes all peer grants
pub const MEM_CAP: u32 = 18; // a0=owned region base -> memory capability
pub const MEM_GRANT: u32 = 19; // memory grant + PMP mapping; one-level R/RW grants

pub const IPC_CALL: u32 = 20; // a0=send handle,a1=req,a2=len,a3=reply buffer,a4=capacity,a5=timeout ticks (0 infinite)
pub const IPC_REPLY: u32 = 21; // a0=single-use reply token,a1=buffer,a2=len
pub const THREAD_PRIORITY: u32 = 22; // a0=own TID,a1=priority 0..3
pub const SPAWN_HELLO: u32 = 23; // boot shell launches bundled ELF (a0: 0=hello, 1=pic, 2=dma), returns PID
pub const EVENT_WAIT: u32 = 24; // a0=boot-bound USB RX event; acknowledge, rearm and wait
pub const DMA_COPY: u32 = 25; // bundled DMA driver: copy a0 bytes between private page halves, block until IRQ
pub const SENTINEL_STATUS: u32 = 26; // a0=acknowledged version (0 until READY), a1=startup epoch
pub const SENTINEL_HELLO: u32 = 27; // blocks for ACK; returns sequence or errno

pub const SENTINEL_QUERY: u32 = 28; // a0=writable 24-byte buffer,a1=size; blocking live GET_STATUS

pub const SENTINEL_CONTROL: u32 = 29; // shell only, a0=0 pause / 1 resume automatic HP heartbeat

pub const SENTINEL_DIAG: u32 = 30; // shell only: a0=code,a1=value; blocking ACK
pub const SENTINEL_LOG: u32 = 31; // a0=sequence (0 metadata),a1=writable 24B,a2=size
// Metadata: epoch,oldest,newest,overwritten,capacity,clock_hz; a1=received diag notices.
// Record: sequence,type,timestamp_lo,timestamp_hi16,arg0,arg1 (LP ticks since LP startup).

pub const SENTINEL_ARM_WAKE: u32 = 32; // a0=1..60000ms; returns LP-acknowledged wake token
pub const SENTINEL_WAIT_WAKE: u32 = 33; // a0=token (0=current own alarm); returns token,a1=elapsed HP 10ms ticks
pub const SENTINEL_CANCEL_WAKE: u32 = 34; // a0=token (0=current own alarm); returns cancel ACK or EALREADY

pub const SENTINEL_PREPARE_SLEEP: u32 = 35; // a0=ms, returns wake token; WAIT/CANCEL use the same ticket

pub const DISPLAY_TEST: u32 = 36; // boot shell only: a0=1 restart bars, 2 restart orbit, 3 restart pixel, 0 advance; returns delay ticks, 0 done, negative error
