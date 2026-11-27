//! Client for Vela's unified file-service IPC protocol (`sysd`, `ext4fs`, `/dev/*`).
use crate::{Buffer, arena::Arena, io, sys};
use vela_abi::{fs, syscall as abi};

pub use vela_abi::fs::{
    DIR_HEADER, KIND_DIR, KIND_FILE, OP_LIST, OP_MKDIR, OP_MOUNT, OP_READ, OP_READ_BULK, OP_STAT,
    OP_UNLINK, OP_UNMOUNT, OP_WRITE, OP_WRITE_BULK, STATUS_ERR, STATUS_NOENT, STATUS_OK,
};

const BULK_CHUNK_MAX: usize = 64 * 1024;

/// Mount a service endpoint (for which the caller holds `GRANT | SEND`) at `prefix`
/// in `sysd`. Requires `CAP_ADMIN` on `vfs_handle`.
pub fn fs_mount(
    vfs_handle: u32,
    prefix: &[u8],
    endpoint_handle: u32,
    response: &mut Buffer,
) -> Result<(), u8> {
    response.clear();
    if prefix.len() < 2
        || prefix.len() > abi::IPC_BYTES - fs::MOUNT_HEADER
        || !prefix.starts_with(b"/")
        || endpoint_handle == 0
    {
        response.push(STATUS_ERR);
        response.append(b"invalid mount request\n");
        return Err(STATUS_ERR);
    }
    let mut request = Buffer::new();
    request.push(OP_MOUNT);
    request.append(prefix);
    let desc = abi::IpcCapCall {
        req_ptr: request.ptr() as u32,
        req_len: request.len() as u32,
        reply_ptr: response.mut_ptr() as u32,
        reply_cap: abi::IPC_BYTES as u32,
        timeout: 500,
        cap_handle: endpoint_handle,
        cap_rights: abi::CAP_SEND,
    };
    let count = sys::ipc_call_cap(vfs_handle, &desc) as usize;
    if !(1..=abi::IPC_BYTES).contains(&count) {
        response.push(STATUS_ERR);
        response.append(b"mount service unavailable\n");
        return Err(STATUS_ERR);
    }
    response.set_len(count);
    let status = response.get(0);
    if status != STATUS_OK {
        return Err(status);
    }
    Ok(())
}

fn fs_path_op(vfs_handle: u32, op: u8, path: &[u8], response: &mut Buffer) -> Result<(), u8> {
    response.clear();
    if path.len() < 2 || path.len() > abi::IPC_BYTES - 1 || !path.starts_with(b"/") {
        response.push(STATUS_ERR);
        response.append(b"invalid file path\n");
        return Err(STATUS_ERR);
    }
    let mut request = Buffer::new();
    request.push(op);
    request.append(path);
    let count = sys::call6(
        abi::IPC_CALL,
        vfs_handle,
        request.ptr() as u32,
        request.len() as u32,
        response.mut_ptr() as u32,
        abi::IPC_BYTES as u32,
        500,
    )
    .0 as usize;
    if !(1..=abi::IPC_BYTES).contains(&count) {
        response.push(STATUS_ERR);
        response.append(b"file service unavailable\n");
        return Err(STATUS_ERR);
    }
    response.set_len(count);
    let status = response.get(0);
    if status != STATUS_OK {
        return Err(status);
    }
    Ok(())
}

/// Remove a regular file or empty directory at `path`.
pub fn fs_unlink(vfs_handle: u32, path: &[u8], response: &mut Buffer) -> Result<(), u8> {
    fs_path_op(vfs_handle, OP_UNLINK, path, response)
}

/// Create a directory at `path` inside an existing parent directory.
pub fn fs_mkdir(vfs_handle: u32, path: &[u8], response: &mut Buffer) -> Result<(), u8> {
    fs_path_op(vfs_handle, OP_MKDIR, path, response)
}

/// Unmount `prefix` in `sysd`. Requires `CAP_ADMIN` on `vfs_handle`.
pub fn fs_unmount(vfs_handle: u32, prefix: &[u8], response: &mut Buffer) -> Result<(), u8> {
    response.clear();
    if prefix.len() < 2
        || prefix.len() > abi::IPC_BYTES - fs::MOUNT_HEADER
        || !prefix.starts_with(b"/")
    {
        response.push(STATUS_ERR);
        response.append(b"invalid mount path\n");
        return Err(STATUS_ERR);
    }
    let mut request = Buffer::new();
    request.push(OP_UNMOUNT);
    request.append(prefix);
    let count = sys::call6(
        abi::IPC_CALL,
        vfs_handle,
        request.ptr() as u32,
        request.len() as u32,
        response.mut_ptr() as u32,
        abi::IPC_BYTES as u32,
        500,
    )
    .0 as usize;
    if !(1..=abi::IPC_BYTES).contains(&count) {
        response.push(STATUS_ERR);
        response.append(b"mount service unavailable\n");
        return Err(STATUS_ERR);
    }
    response.set_len(count);
    let status = response.get(0);
    if status != STATUS_OK {
        return Err(status);
    }
    Ok(())
}

/// Query `(size, kind)` for `path` without printing diagnostics.
pub fn fs_stat_raw(handle: u32, path: &[u8], response: &mut Buffer) -> Result<(u32, u8), u8> {
    response.clear();
    if path.is_empty() || path.len() > abi::IPC_BYTES - fs::STAT_HEADER || !path.starts_with(b"/") {
        response.push(STATUS_ERR);
        response.append(b"invalid file path\n");
        return Err(STATUS_ERR);
    }
    let mut request = Buffer::new();
    request.push(OP_STAT);
    request.append(path);
    let count = sys::call6(
        abi::IPC_CALL,
        handle,
        request.ptr() as u32,
        request.len() as u32,
        response.mut_ptr() as u32,
        abi::IPC_BYTES as u32,
        500,
    )
    .0 as usize;
    if !(1..=abi::IPC_BYTES).contains(&count) {
        response.push(STATUS_ERR);
        response.append(b"file service unavailable\n");
        return Err(STATUS_ERR);
    }
    response.set_len(count);
    let status = response.get(0);
    if status != STATUS_OK {
        return Err(status);
    }
    let s = response.as_slice();
    if s.len() < fs::STAT_REPLY_BYTES {
        response.clear();
        response.push(STATUS_ERR);
        response.append(b"invalid stat reply\n");
        return Err(STATUS_ERR);
    }
    let size = u32::from_le_bytes([s[1], s[2], s[3], s[4]]);
    let kind = s[5];
    Ok((size, kind))
}

/// Bulk-read up to `dst.len()` bytes from `path` starting at `offset` directly into `dst`.
/// Returns `Ok(bytes_read)` (`0` on EOF).
pub fn fs_read_bulk(
    handle: u32,
    path: &[u8],
    offset: u32,
    dst: &mut [u8],
    response: &mut Buffer,
) -> Result<usize, u8> {
    fs_read_bulk_mode(handle, path, offset, dst, response, true)
}

/// Explicit single-sector ext4 comparison, preserving the bulk RPC chunk size.
pub fn fs_read_bulk_mode(
    handle: u32,
    path: &[u8],
    offset: u32,
    dst: &mut [u8],
    response: &mut Buffer,
    batch: bool,
) -> Result<usize, u8> {
    fs_read_bulk_request(
        handle,
        path,
        offset,
        dst,
        response,
        if batch {
            OP_READ_BULK
        } else {
            fs::OP_READ_BULK_SINGLE
        },
    )
}

/// Comparison path retaining the service staging buffer with batched SD reads.
pub fn fs_read_bulk_staged(
    handle: u32,
    path: &[u8],
    offset: u32,
    dst: &mut [u8],
    response: &mut Buffer,
) -> Result<usize, u8> {
    fs_read_bulk_request(handle, path, offset, dst, response, fs::OP_READ_BULK_STAGED)
}

/// Request service and kernel timings for an explicit bulk-read operation.
pub fn fs_read_bulk_profiled(
    handle: u32,
    path: &[u8],
    offset: u32,
    dst: &mut [u8],
    response: &mut Buffer,
    operation: u8,
) -> Result<usize, u8> {
    if fs::bulk_read_operation(operation).is_none() {
        response.clear();
        response.push(STATUS_ERR);
        response.append(b"invalid profiled read operation");
        return Err(STATUS_ERR);
    }
    fs_read_bulk_request(
        handle,
        path,
        offset,
        dst,
        response,
        operation | fs::BULK_READ_PROFILE,
    )
}

fn fs_read_bulk_request(
    handle: u32,
    path: &[u8],
    offset: u32,
    dst: &mut [u8],
    response: &mut Buffer,
    operation: u8,
) -> Result<usize, u8> {
    response.clear();
    if dst.is_empty() {
        return Ok(0);
    }
    if path.is_empty() || path.len() > fs::MAX_BULK_PATH || !path.starts_with(b"/") {
        response.push(STATUS_ERR);
        response.append(b"invalid file path\n");
        return Err(STATUS_ERR);
    }
    let want = dst.len().min(abi::IPC_BULK_MAX) as u32;
    let mut request = Buffer::new();
    request.push(operation);
    request.append(&offset.to_le_bytes());
    request.append(&want.to_le_bytes());
    request.append(path);
    let desc = abi::IpcBulkCall {
        req_ptr: request.ptr() as u32,
        req_len: request.len() as u32,
        reply_ptr: response.mut_ptr() as u32,
        reply_cap: abi::IPC_BYTES as u32,
        timeout: 1500,
        bulk_ptr: dst.as_mut_ptr() as u32,
        bulk_len: want,
        bulk_flags: abi::IPC_BULK_WRITE,
        forward_token: 0,
    };
    let count = sys::ipc_call_bulk(handle, &desc) as usize;
    if !(1..=abi::IPC_BYTES).contains(&count) {
        response.push(STATUS_ERR);
        response.append(b"file service unavailable\n");
        return Err(STATUS_ERR);
    }
    response.set_len(count);
    let status = response.get(0);
    if status != STATUS_OK {
        return Err(status);
    }
    let s = response.as_slice();
    if s.len() < fs::BULK_READ_REPLY_BYTES {
        response.clear();
        response.push(STATUS_ERR);
        response.append(b"invalid bulk read reply\n");
        return Err(STATUS_ERR);
    }
    let read = u32::from_le_bytes([s[1], s[2], s[3], s[4]]) as usize;
    if read > dst.len() {
        response.clear();
        response.push(STATUS_ERR);
        response.append(b"invalid bulk read length\n");
        return Err(STATUS_ERR);
    }
    Ok(read)
}

/// Load a whole file into a newly allocated PSRAM (or SRAM fallback) `Arena`
/// using `OP_STAT` + `OP_READ_BULK`.
#[inline(never)]
pub fn fs_load_arena(
    handle: u32,
    path: &[u8],
    max_bytes: usize,
    response: &mut Buffer,
) -> Result<(Arena, usize), u8> {
    let (size_u32, kind) = fs_stat_raw(handle, path, response)?;
    let size = size_u32 as usize;
    if kind != KIND_FILE || size == 0 || size > max_bytes {
        response.clear();
        response.push(STATUS_ERR);
        if kind != KIND_FILE {
            response.append(b"not a regular file\n");
        } else if size == 0 {
            response.append(b"empty file\n");
        } else {
            response.append(b"file exceeds size limit\n");
        }
        return Err(STATUS_ERR);
    }
    let Ok(mut arena) = Arena::new_psram(size).or_else(|_| Arena::new(size)) else {
        response.clear();
        response.push(STATUS_ERR);
        response.append(b"out of memory\n");
        return Err(STATUS_ERR);
    };
    let mut loaded = 0usize;
    while loaded < size {
        let step = (size - loaded).min(BULK_CHUNK_MAX);
        // SAFETY: `arena` is privately owned and `loaded + step <= size <= arena.capacity()`.
        let dst =
            unsafe { core::slice::from_raw_parts_mut((arena.base() + loaded) as *mut u8, step) };
        match fs_read_bulk(handle, path, loaded as u32, dst, response) {
            Ok(0) => break,
            Ok(n) => loaded += n,
            Err(status) => {
                let _ = arena.release();
                return Err(status);
            }
        }
    }
    if loaded == 0 {
        let _ = arena.release();
        response.clear();
        response.push(STATUS_ERR);
        response.append(b"empty file\n");
        return Err(STATUS_ERR);
    }
    Ok((arena, loaded))
}

/// Read a chunk from `path` at `offset` without printing error diagnostics.
/// On `Err(status)`, `response.as_slice()[1..]` holds any server message when `status != 0`.
pub fn fs_read_raw(
    handle: u32,
    path: &[u8],
    offset: u32,
    response: &mut Buffer,
) -> Result<usize, u8> {
    fs_pos_request(handle, OP_READ, path, offset, response).map(|len| len - 1)
}

/// Read a chunk from `path` at `offset`, printing any server error diagnostic.
pub fn fs_read(handle: u32, path: &[u8], offset: u32, response: &mut Buffer) -> Result<usize, ()> {
    match fs_read_raw(handle, path, offset, response) {
        Ok(len) => Ok(len),
        Err(_) => {
            if response.len() > 1 {
                io::ewrite(&response.as_slice()[1..]);
            }
            Err(())
        }
    }
}

/// A successful directory reply is either status alone (EOF) or a header and
/// raw name bytes. The cursor belongs to the same path and mount.
pub fn fs_read_dir(
    handle: u32,
    path: &[u8],
    cursor: u32,
    response: &mut Buffer,
) -> Result<Option<(u32, bool)>, ()> {
    let len = match fs_pos_request(handle, OP_LIST, path, cursor, response) {
        Ok(len) => len,
        Err(_) => {
            if response.len() > 1 {
                io::ewrite(&response.as_slice()[1..]);
            }
            return Err(());
        }
    };
    match response.as_slice() {
        [STATUS_OK] => Ok(None),
        [STATUS_OK, a, b, c, d, end @ 0..=1, ..] if len > DIR_HEADER => {
            let next = u32::from_le_bytes([*a, *b, *c, *d]);
            if next > cursor {
                return Ok(Some((next, *end == 1)));
            }
            io::ewrite(b"invalid directory reply\n");
            Err(())
        }
        _ => {
            io::ewrite(b"invalid directory reply\n");
            Err(())
        }
    }
}

/// Write `data` to `path` at `offset`. Returns the number of optional reply bytes in
/// `response.as_slice()[1..]`, or prints the diagnostic and returns `Err(())`.
pub fn fs_write(
    handle: u32,
    path: &[u8],
    offset: u32,
    data: &[u8],
    response: &mut Buffer,
) -> Result<usize, ()> {
    if path.is_empty()
        || !path.starts_with(b"/")
        || path.len() > u8::MAX as usize
        || fs::WRITE_HEADER + path.len() + data.len() > abi::IPC_BYTES
    {
        io::ewrite(b"invalid write request\n");
        return Err(());
    }
    let mut request = Buffer::new();
    request.push(OP_WRITE);
    request.append(&offset.to_le_bytes());
    request.push(path.len() as u8);
    request.append(path);
    request.append(data);
    // Timeout 0 lets blocking device writes (e.g. `/sys/sentinel/sleep`) complete.
    let count = sys::call6(
        abi::IPC_CALL,
        handle,
        request.ptr() as u32,
        request.len() as u32,
        response.mut_ptr() as u32,
        abi::IPC_BYTES as u32,
        0,
    )
    .0 as usize;
    if !(1..=abi::IPC_BYTES).contains(&count) {
        io::ewrite(b"file service unavailable\n");
        return Err(());
    }
    response.set_len(count);
    if response.get(0) != STATUS_OK {
        io::ewrite(&response.as_slice()[1..]);
        return Err(());
    }
    Ok(count - 1)
}

/// Write arbitrary-sized `data` to `path` at `offset` using `OP_WRITE_BULK` (or
/// inline `OP_WRITE` when the payload is empty or fits inside a single IPC frame).
pub fn fs_write_bulk(
    handle: u32,
    path: &[u8],
    offset: u32,
    data: &[u8],
    response: &mut Buffer,
) -> Result<usize, ()> {
    if data.is_empty() || fs::WRITE_HEADER + path.len() + data.len() <= abi::IPC_BYTES {
        fs_write(handle, path, offset, data, response)?;
        return Ok(data.len());
    }
    if path.is_empty() || path.len() > fs::MAX_BULK_PATH || !path.starts_with(b"/") {
        io::ewrite(b"invalid write path\n");
        return Err(());
    }
    let mut written = 0usize;
    while written < data.len() {
        let chunk_len = (data.len() - written).min(BULK_CHUNK_MAX);
        let chunk = &data[written..written + chunk_len];
        let mut request = Buffer::new();
        request.push(OP_WRITE_BULK);
        request.append(&(offset.saturating_add(written as u32)).to_le_bytes());
        request.append(&(chunk_len as u32).to_le_bytes());
        request.append(path);
        let desc = abi::IpcBulkCall {
            req_ptr: request.ptr() as u32,
            req_len: request.len() as u32,
            reply_ptr: response.mut_ptr() as u32,
            reply_cap: abi::IPC_BYTES as u32,
            timeout: 0,
            bulk_ptr: chunk.as_ptr() as u32,
            bulk_len: chunk_len as u32,
            bulk_flags: abi::IPC_BULK_READ,
            forward_token: 0,
        };
        response.clear();
        let count = sys::ipc_call_bulk(handle, &desc) as usize;
        if !(1..=abi::IPC_BYTES).contains(&count) {
            io::ewrite(b"file service unavailable\n");
            return Err(());
        }
        response.set_len(count);
        if response.get(0) != STATUS_OK {
            io::ewrite(&response.as_slice()[1..]);
            return Err(());
        }
        let s = response.as_slice();
        if s.len() < fs::BULK_WRITE_REPLY_BYTES {
            io::ewrite(b"invalid bulk write reply\n");
            return Err(());
        }
        let n = u32::from_le_bytes([s[1], s[2], s[3], s[4]]) as usize;
        if n == 0 || n > chunk_len {
            io::ewrite(b"short bulk write\n");
            return Err(());
        }
        written += n;
    }
    Ok(written)
}

fn fs_pos_request(
    handle: u32,
    operation: u8,
    path: &[u8],
    offset: u32,
    response: &mut Buffer,
) -> Result<usize, u8> {
    response.clear();
    if path.len() > fs::MAX_PATH || !path.starts_with(b"/") {
        response.push(STATUS_ERR);
        response.append(b"invalid file path\n");
        return Err(STATUS_ERR);
    }
    let mut request = Buffer::new();
    request.push(operation);
    request.append(&offset.to_le_bytes());
    request.append(path);
    let count = sys::call6(
        abi::IPC_CALL,
        handle,
        request.ptr() as u32,
        request.len() as u32,
        response.mut_ptr() as u32,
        abi::IPC_BYTES as u32,
        500,
    )
    .0 as usize;
    if !(1..=abi::IPC_BYTES).contains(&count) {
        response.push(STATUS_ERR);
        response.append(b"file service unavailable\n");
        return Err(STATUS_ERR);
    }
    response.set_len(count);
    let status = response.get(0);
    if status != STATUS_OK {
        return Err(status);
    }
    Ok(count)
}

pub fn cat(handle: u32, path: &[u8]) {
    let mut response = Buffer::new();
    let Ok(mut arena) = Arena::new_psram(BULK_CHUNK_MAX) else {
        io::ewrite(b"cat: read buffer allocation failed\n");
        return;
    };
    // SAFETY: private live allocation covers the buffer; no access after release.
    let chunk = unsafe { core::slice::from_raw_parts_mut(arena.base() as *mut u8, BULK_CHUNK_MAX) };
    let mut offset = 0u32;
    loop {
        match fs_read_bulk(handle, path, offset, chunk, &mut response) {
            Ok(0) => break,
            Ok(count) => {
                if io::write(&chunk[..count]).is_err() {
                    break;
                }
                offset = offset.saturating_add(count as u32);
            }
            Err(_) => {
                if response.len() > 1 {
                    io::ewrite(&response.as_slice()[1..]);
                }
                break;
            }
        }
    }
    let _ = arena.release();
}
