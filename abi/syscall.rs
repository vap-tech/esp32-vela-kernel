//! Initial Vela RV32 ABI: a7=number, a0..a2=args, a0=result (negative errno).
//! No kernel pointers cross the ABI. Numbers are append-only.
pub const WRITE: u32 = 1; // a0=readable user buffer, a1=len, <=128 bytes
pub const YIELD: u32 = 2;
pub const EXIT: u32 = 3;
pub const TICKS: u32 = 4; // low 32 bits of 100Hz uptime; wrap after ~497 days
pub const STATS: u32 = 5; // a0=writable user buffer; explicit wire layout below
pub const STATS_BYTES: usize = 44; // 4*u32, 2*u64, 3*u32, little endian
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
