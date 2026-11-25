#![no_std]
#![no_main]
//! Dynamically loadable userspace RAM-backed character device driver (`/sd/bin/ramdev.elf`).
//!
//! Usage from shell: `start /sd/bin/ramdev.elf [/dev/scratch]` (defaults to `/dev/ram0`).
//! Creates an IPC endpoint, registers it with `sysd` via `OP_MOUNT` (`IPC_CALL_CAP`),
//! and serves `OP_STAT`, `OP_READ`, `OP_READ_BULK`, and `OP_WRITE` over a 4 KiB buffer.

use vela_abi::{fs as fs_abi, syscall as abi};
use vela_user::{Buffer, file, sys};

vela_user::entry!(main);

const CAPACITY: usize = 4096;
static mut STORE: [u8; CAPACITY] = [0; CAPACITY];

fn handle_request(
    pending_token: u32,
    request: &Buffer,
    store: &mut [u8; CAPACITY],
    used_len: &mut usize,
    response: &mut Buffer,
) {
    response.clear();
    let req = request.as_slice();
    match req {
        [fs_abi::OP_STAT, path @ ..] if path == b"/" => {
            response.push(fs_abi::STATUS_OK);
            response.append(&(*used_len as u32).to_le_bytes());
            response.push(fs_abi::KIND_FILE);
        }
        [fs_abi::OP_READ, a, b, c, d, path @ ..] if path == b"/" => {
            let offset = u32::from_le_bytes([*a, *b, *c, *d]) as usize;
            response.push(fs_abi::STATUS_OK);
            if offset < *used_len {
                let end = (offset + (abi::IPC_BYTES - 1)).min(*used_len);
                response.append(&store[offset..end]);
            }
        }
        [fs_abi::OP_READ_BULK, a, b, c, d, e, f, g, h, path @ ..] if path == b"/" => {
            let offset = u32::from_le_bytes([*a, *b, *c, *d]) as usize;
            let max_len = u32::from_le_bytes([*e, *f, *g, *h]) as usize;
            if offset >= *used_len || max_len == 0 {
                response.push(fs_abi::STATUS_OK);
                response.append(&0u32.to_le_bytes());
                return;
            }
            let end = (offset + max_len).min(*used_len);
            let slice = &store[offset..end];
            if sys::ipc_bulk_write(pending_token, 0, slice).is_err() {
                response.push(fs_abi::STATUS_ERR);
                response.append(b"ramdev: bulk write failed\n");
                return;
            }
            response.push(fs_abi::STATUS_OK);
            response.append(&(slice.len() as u32).to_le_bytes());
        }
        [fs_abi::OP_WRITE, a, b, c, d, path_len, rest @ ..]
            if usize::from(*path_len) == 1 && rest.starts_with(b"/") =>
        {
            let offset = u32::from_le_bytes([*a, *b, *c, *d]) as usize;
            let data = &rest[1..];
            let Some(end) = offset.checked_add(data.len()) else {
                response.push(fs_abi::STATUS_ERR);
                response.append(b"ramdev: offset overflow\n");
                return;
            };
            if end > CAPACITY {
                response.push(fs_abi::STATUS_ERR);
                response.append(b"ramdev: buffer full\n");
                return;
            }
            store[offset..end].copy_from_slice(data);
            if offset == 0 {
                *used_len = end;
            } else if end > *used_len {
                *used_len = end;
            }
            response.push(fs_abi::STATUS_OK);
        }
        [fs_abi::OP_LIST, ..] => {
            response.push(fs_abi::STATUS_ERR);
            response.append(b"ramdev: not a directory\n");
        }
        _ => {
            response.push(fs_abi::STATUS_NOENT);
            response.append(b"ramdev: no such entry\n");
        }
    }
}

fn main(vfs: u32, arg: u32, len: u32, _a3: u32, _a4: u32, _a5: u32) -> ! {
    if vfs == 0 || len as usize > abi::SPAWN_ARG_MAX || (len != 0 && arg == 0) {
        sys::write(b"ramdev: missing vfs capability\n");
        sys::process_exit();
    }
    let prefix = if len == 0 {
        b"/dev/ram0".as_slice()
    } else {
        // SAFETY: SPAWN_IMAGE places this read-only argument above initial SP.
        let text =
            unsafe { core::slice::from_raw_parts(arg as *const u8, len as usize) }.trim_ascii();
        if text.is_empty() {
            b"/dev/ram0".as_slice()
        } else {
            text
        }
    };
    let Ok(endpoint) = sys::endpoint_create() else {
        sys::write(b"ramdev: endpoint_create failed\n");
        sys::process_exit();
    };
    let mut response = Buffer::new();
    if file::fs_mount(vfs, prefix, endpoint, &mut response).is_err() {
        if response.len() > 1 {
            sys::write(&response.as_slice()[1..]);
        } else {
            sys::write(b"ramdev: mount failed\n");
        }
        let _ = sys::endpoint_close(endpoint);
        sys::process_exit();
    }

    // SAFETY: single-threaded driver, `STORE` is never aliased.
    let store = unsafe { &mut *core::ptr::addr_of_mut!(STORE) };
    let mut used_len = 0usize;
    let mut request = Buffer::new();
    let mut pending_token = 0u32;
    loop {
        let ok = if pending_token != 0 {
            request.reply_and_receive(pending_token, &response, endpoint)
        } else {
            request.receive(endpoint)
        };
        if !ok {
            sys::process_exit();
        }
        pending_token = request.reply;
        if pending_token == 0 {
            continue;
        }
        handle_request(pending_token, &request, store, &mut used_len, &mut response);
    }
}
