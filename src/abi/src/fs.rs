//! Unified userspace file-service IPC wire format shared by `sysd`, `ext4fs`,
//! future `/dev/*` drivers, and clients (`shell`, `cat`, `ls`).
//!
//! All requests carry a leading operation byte:
//! - `OP_READ`:  `[0, offset: u32 LE, path..]` -> `[STATUS_OK, bytes..]` (empty payload = EOF)
//! - `OP_LIST`:  `[1, cursor: u32 LE, path..]` -> `[STATUS_OK]` (EOF) or
//!               `[STATUS_OK, next_cursor: u32 LE, end_of_name: u8, name_bytes..]`
//! - `OP_WRITE`: `[2, offset: u32 LE, path_len: u8, path[..path_len], data..]` ->
//!               `[STATUS_OK, optional_reply..]`
//!
//! Any reply with `status != STATUS_OK` carries a human-readable diagnostic in `reply[1..]`.

use crate::syscall::IPC_BYTES;

pub const OP_READ: u8 = 0;
pub const OP_LIST: u8 = 1;
pub const OP_WRITE: u8 = 2;

/// Reply status codes.
pub const STATUS_OK: u8 = 0;
pub const STATUS_NOENT: u8 = 1;
pub const STATUS_ERR: u8 = 2;

/// Header bytes for `OP_LIST` replies: `[status: u8, next: u32 LE, end_of_name: u8]`.
pub const DIR_HEADER: usize = 6;

/// Header bytes for `OP_READ` / `OP_LIST` requests: `[op: u8, pos: u32 LE]`.
pub const REQ_HEADER: usize = 5;

/// Header bytes for `OP_WRITE` requests: `[op: u8, offset: u32 LE, path_len: u8]`.
pub const WRITE_HEADER: usize = 6;

/// Maximum path bytes accepted in `OP_READ` and `OP_LIST` requests.
pub const MAX_PATH: usize = IPC_BYTES - REQ_HEADER;
