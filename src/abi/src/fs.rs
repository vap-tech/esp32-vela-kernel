//! Unified userspace file-service IPC wire format shared by `sysd`, `ext4fs`,
//! dynamically mounted `/dev/*` drivers, and clients (`shell`, `cat`, `ls`, `init`).
//!
//! All requests carry a leading operation byte:
//! - `OP_READ`:      `[0, offset: u32 LE, path..]` -> `[STATUS_OK, bytes..]` (empty = EOF)
//! - `OP_LIST`:      `[1, cursor: u32 LE, path..]` -> `[STATUS_OK]` (EOF) or
//!                   `[STATUS_OK, next_cursor: u32 LE, end_of_name: u8, name_bytes..]`
//! - `OP_WRITE`:     `[2, offset: u32 LE, path_len: u8, path[..path_len], data..]` ->
//!                   `[STATUS_OK, optional_reply..]`
//! - `OP_STAT`:      `[3, path..]` -> `[STATUS_OK, size: u32 LE, kind: u8]`
//! - `OP_READ_BULK`: `[4, offset: u32 LE, max_len: u32 LE, path..]` via `IPC_CALL_BULK`
//!                   (`IPC_BULK_WRITE`) -> `[STATUS_OK, bytes_read: u32 LE]`
//! - `OP_MOUNT`:     `[5, prefix..]` via `IPC_CALL_CAP` attaching `(endpoint, CAP_SEND)`
//!                   (requires `CAP_ADMIN` on the `sysd` VFS handle) -> `[STATUS_OK]`
//! - `OP_UNMOUNT`:   `[6, prefix..]` via `IPC_CALL`
//!                   (requires `CAP_ADMIN` on the `sysd` VFS handle) -> `[STATUS_OK]`
//! - `OP_WRITE_BULK`: `[7, offset: u32 LE, len: u32 LE, path..]` via `IPC_CALL_BULK`
//!                   (`IPC_BULK_READ`) -> `[STATUS_OK, bytes_written: u32 LE]`
//!
//! Any reply with `status != STATUS_OK` carries a human-readable diagnostic in `reply[1..]`.

use crate::syscall::IPC_BYTES;

pub const OP_READ: u8 = 0;
pub const OP_LIST: u8 = 1;
pub const OP_WRITE: u8 = 2;
pub const OP_STAT: u8 = 3;
pub const OP_READ_BULK: u8 = 4;
pub const OP_MOUNT: u8 = 5;
pub const OP_UNMOUNT: u8 = 6;
pub const OP_WRITE_BULK: u8 = 7;

/// Reply status codes.
pub const STATUS_OK: u8 = 0;
pub const STATUS_NOENT: u8 = 1;
pub const STATUS_ERR: u8 = 2;

/// Entry kinds returned by `OP_STAT`.
pub const KIND_FILE: u8 = 1;
pub const KIND_DIR: u8 = 2;

/// Header bytes for `OP_LIST` replies: `[status: u8, next: u32 LE, end_of_name: u8]`.
pub const DIR_HEADER: usize = 6;

/// Header bytes for `OP_READ` / `OP_LIST` requests: `[op: u8, pos: u32 LE]`.
pub const REQ_HEADER: usize = 5;

/// Header bytes for `OP_WRITE` requests: `[op: u8, offset: u32 LE, path_len: u8]`.
pub const WRITE_HEADER: usize = 6;

/// Header bytes for `OP_STAT` requests: `[op: u8]`.
pub const STAT_HEADER: usize = 1;

/// Byte length of a successful `OP_STAT` reply: `[status: u8, size: u32 LE, kind: u8]`.
pub const STAT_REPLY_BYTES: usize = 6;

/// Header bytes for `OP_READ_BULK` requests: `[op: u8, offset: u32 LE, max_len: u32 LE]`.
pub const BULK_READ_HEADER: usize = 9;

/// Byte length of a successful `OP_READ_BULK` reply: `[status: u8, bytes_read: u32 LE]`.
pub const BULK_READ_REPLY_BYTES: usize = 5;

/// Header bytes for `OP_WRITE_BULK` requests: `[op: u8, offset: u32 LE, len: u32 LE]`.
pub const BULK_WRITE_HEADER: usize = 9;

/// Byte length of a successful `OP_WRITE_BULK` reply: `[status: u8, bytes_written: u32 LE]`.
pub const BULK_WRITE_REPLY_BYTES: usize = 5;

/// Header bytes for `OP_MOUNT` / `OP_UNMOUNT` requests: `[op: u8]`.
pub const MOUNT_HEADER: usize = 1;

/// Maximum path bytes accepted in `OP_READ` and `OP_LIST` requests.
pub const MAX_PATH: usize = IPC_BYTES - REQ_HEADER;

/// Maximum path bytes accepted in `OP_READ_BULK` requests.
pub const MAX_BULK_PATH: usize = IPC_BYTES - BULK_READ_HEADER;
