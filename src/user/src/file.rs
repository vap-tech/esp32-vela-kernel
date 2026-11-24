//! Client for the ext4 file service. Paths are relative to the mounted filesystem.
use crate::{Buffer, sys};
use vela_abi::syscall as abi;

pub fn ext4_read(
    handle: u32,
    path: &[u8],
    offset: u32,
    response: &mut Buffer,
) -> Result<usize, ()> {
    if path.len() > abi::IPC_BYTES - 5 || !path.starts_with(b"/") {
        sys::write(b"invalid file path\n");
        return Err(());
    }
    let mut request = Buffer::new();
    request.push(0);
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
        sys::write(b"file service unavailable\n");
        return Err(());
    }
    response.set_len(count);
    if response.get(0) != 0 {
        sys::write(&response.as_slice()[1..]);
        return Err(());
    }
    Ok(count - 1)
}

pub fn cat_ext4(handle: u32, path: &[u8]) {
    let mut response = Buffer::new();
    let mut offset = 0;
    while let Ok(count) = ext4_read(handle, path, offset, &mut response) {
        if count == 0 {
            break;
        }
        sys::write(&response.as_slice()[1..]);
        offset += count as u32;
    }
}
