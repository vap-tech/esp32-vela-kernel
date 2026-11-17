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
