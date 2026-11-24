//! Client for Vela's unified file-service IPC protocol (`sysd`, `ext4fs`, `/dev/*`).
use crate::{Buffer, sys};
use vela_abi::{fs, syscall as abi};

pub use vela_abi::fs::{
    DIR_HEADER, OP_LIST, OP_READ, OP_WRITE, STATUS_ERR, STATUS_NOENT, STATUS_OK,
};

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
                sys::write(&response.as_slice()[1..]);
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
                sys::write(&response.as_slice()[1..]);
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
            sys::write(b"invalid directory reply\n");
            Err(())
        }
        _ => {
            sys::write(b"invalid directory reply\n");
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
        sys::write(b"invalid write request\n");
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
        sys::write(b"file service unavailable\n");
        return Err(());
    }
    response.set_len(count);
    if response.get(0) != STATUS_OK {
        sys::write(&response.as_slice()[1..]);
        return Err(());
    }
    Ok(count - 1)
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
    let mut offset = 0;
    while let Ok(count) = fs_read(handle, path, offset, &mut response) {
        if count == 0 {
            break;
        }
        sys::write(&response.as_slice()[1..]);
        offset += count as u32;
    }
}
