#![no_std]
#![no_main]
//! `sysd`: root userspace namespace service for `/proc`, `/sys`, `/dev`, and mounted filesystems (`/sd`).

mod dev;
mod proc;
mod sys;

use vela_abi::{fs as fs_abi, syscall as abi};
use vela_user::{
    Buffer, TextBuf,
    mount::{self, MountTable},
    sys as usys,
};

vela_user::entry!(main);

const ROOT_ENTRIES: &[&[u8]] = &[b"dev", b"proc", b"sd", b"sys"];

#[derive(Clone, Copy)]
struct Caps {
    dev: dev::DevCaps,
    sys: sys::SysCaps,
}

struct SnapshotCache {
    path: [u8; 48],
    path_len: usize,
    status: u8,
    text: TextBuf<2048>,
}

/// NOLOAD `.bss` cache so `sysd`'s 4 KiB stack never holds the 2 KiB render buffer.
static mut CACHE: SnapshotCache = SnapshotCache::new();

impl SnapshotCache {
    const fn new() -> Self {
        Self {
            path: [0; 48],
            path_len: 0,
            status: fs_abi::STATUS_NOENT,
            text: TextBuf::new(),
        }
    }

    fn invalidate(&mut self) {
        self.path_len = 0;
    }

    fn render_or_reuse(&mut self, offset: usize, path: &[u8], caps: Caps) -> (u8, &[u8]) {
        let same_path =
            offset > 0 && path.len() == self.path_len && self.path[..self.path_len] == *path;
        if !same_path {
            self.text.clear();
            self.status = render_virtual(path, caps, &mut self.text);
            if path.len() <= self.path.len() {
                self.path[..path.len()].copy_from_slice(path);
                self.path_len = path.len();
            } else {
                self.path_len = 0;
            }
        }
        (self.status, self.text.as_slice())
    }
}

fn is_virtual_dir(norm: &[u8]) -> bool {
    matches!(norm, b"/" | b"/proc" | b"/sys" | b"/dev" | b"/sys/sentinel")
}

#[inline(never)]
fn render_virtual(path: &[u8], caps: Caps, out: &mut TextBuf<2048>) -> u8 {
    let norm = mount::normalize_path(path);
    if let Some(rel) = norm.strip_prefix(b"/proc/") {
        return proc::read(rel, out);
    }
    if let Some(rel) = norm.strip_prefix(b"/sys/") {
        return sys::read(rel, caps.sys, out);
    }
    if let Some(rel) = norm.strip_prefix(b"/dev/") {
        return dev::read(rel, caps.dev, out);
    }
    if is_virtual_dir(norm) {
        out.append(b"sysd: is a directory\n");
        return fs_abi::STATUS_ERR;
    }
    out.append(b"sysd: no such file\n");
    fs_abi::STATUS_NOENT
}

fn handle_stat(
    path: &[u8],
    mounts: &MountTable,
    caps: Caps,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    if let Some((ep, rel)) = mounts.resolve(path) {
        mount::forward_stat(ep, rel, response);
        return;
    }
    let norm = mount::normalize_path(path);
    if is_virtual_dir(norm) {
        response.push(fs_abi::STATUS_OK);
        response.append(&0u32.to_le_bytes());
        response.push(fs_abi::KIND_DIR);
        return;
    }
    let (status, bytes) = cache.render_or_reuse(0, path, caps);
    if status != fs_abi::STATUS_OK {
        response.push(status);
        response.append(bytes);
        return;
    }
    response.push(fs_abi::STATUS_OK);
    response.append(&(bytes.len() as u32).to_le_bytes());
    response.push(fs_abi::KIND_FILE);
}

fn handle_read(
    offset: usize,
    path: &[u8],
    mounts: &MountTable,
    caps: Caps,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    if let Some((ep, rel)) = mounts.resolve(path) {
        mount::forward_pos(ep, fs_abi::OP_READ, offset as u32, rel, response);
        return;
    }
    let (status, bytes) = cache.render_or_reuse(offset, path, caps);
    response.push(status);
    if status != fs_abi::STATUS_OK {
        response.append(bytes);
        return;
    }
    if offset >= bytes.len() {
        return; // EOF: empty payload after STATUS_OK
    }
    let end = (offset + (abi::IPC_BYTES - 1)).min(bytes.len());
    response.append(&bytes[offset..end]);
}

fn handle_read_bulk(
    pending_token: u32,
    offset: usize,
    max_len: usize,
    path: &[u8],
    mounts: &MountTable,
    caps: Caps,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    if let Some((ep, rel)) = mounts.resolve(path) {
        mount::forward_read_bulk(
            ep,
            pending_token,
            offset as u32,
            max_len as u32,
            rel,
            response,
        );
        return;
    }
    let (status, bytes) = cache.render_or_reuse(offset, path, caps);
    if status != fs_abi::STATUS_OK {
        response.push(status);
        response.append(bytes);
        return;
    }
    if offset >= bytes.len() || max_len == 0 {
        response.push(fs_abi::STATUS_OK);
        response.append(&0u32.to_le_bytes());
        return;
    }
    let end = (offset + max_len).min(bytes.len());
    let slice = &bytes[offset..end];
    if usys::ipc_bulk_write(pending_token, 0, slice).is_err() {
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: bulk write failed\n");
        return;
    }
    response.push(fs_abi::STATUS_OK);
    response.append(&(slice.len() as u32).to_le_bytes());
}

fn handle_list(cursor: u32, path: &[u8], mounts: &MountTable, response: &mut Buffer) {
    if let Some((ep, rel)) = mounts.resolve(path) {
        mount::forward_pos(ep, fs_abi::OP_LIST, cursor, rel, response);
        return;
    }
    match mount::normalize_path(path) {
        b"/" => mount::list_static(ROOT_ENTRIES, cursor, response),
        b"/proc" => mount::list_static(proc::ENTRIES, cursor, response),
        b"/sys" => mount::list_static(sys::ENTRIES, cursor, response),
        b"/sys/sentinel" => mount::list_static(sys::SENTINEL_ENTRIES, cursor, response),
        b"/dev" => mount::list_static(dev::ENTRIES, cursor, response),
        _ => {
            response.push(fs_abi::STATUS_NOENT);
            response.append(b"sysd: not a directory\n");
        }
    }
}

fn handle_write(
    offset: u32,
    path: &[u8],
    data: &[u8],
    mounts: &MountTable,
    caps: Caps,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    cache.invalidate();
    if let Some((ep, rel)) = mounts.resolve(path) {
        mount::forward_write(ep, offset, rel, data, response);
        return;
    }
    let norm = mount::normalize_path(path);
    cache.text.clear();
    let status = if let Some(rel) = norm.strip_prefix(b"/sys/") {
        sys::write(rel, data, caps.sys, &mut cache.text)
    } else if let Some(rel) = norm.strip_prefix(b"/dev/") {
        dev::write(rel, data, caps.dev, &mut cache.text)
    } else if norm.starts_with(b"/proc/") {
        cache.text.append(b"proc: read-only filesystem\n");
        fs_abi::STATUS_ERR
    } else {
        cache.text.append(b"sysd: no such file\n");
        fs_abi::STATUS_NOENT
    };
    response.push(status);
    response.append(cache.text.as_slice());
}

fn handle_request(
    pending_token: u32,
    request: &Buffer,
    mounts: &MountTable,
    caps: Caps,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    response.clear();
    let req = request.as_slice();
    match req {
        [fs_abi::OP_READ, a, b, c, d, path @ ..] if !path.is_empty() => {
            let offset = u32::from_le_bytes([*a, *b, *c, *d]) as usize;
            handle_read(offset, path, mounts, caps, cache, response);
        }
        [fs_abi::OP_READ_BULK, a, b, c, d, e, f, g, h, path @ ..] if !path.is_empty() => {
            let offset = u32::from_le_bytes([*a, *b, *c, *d]) as usize;
            let max_len = u32::from_le_bytes([*e, *f, *g, *h]) as usize;
            handle_read_bulk(
                pending_token,
                offset,
                max_len,
                path,
                mounts,
                caps,
                cache,
                response,
            );
        }
        [fs_abi::OP_STAT, path @ ..] if !path.is_empty() => {
            handle_stat(path, mounts, caps, cache, response);
        }
        [fs_abi::OP_LIST, a, b, c, d, path @ ..] if !path.is_empty() => {
            let cursor = u32::from_le_bytes([*a, *b, *c, *d]);
            handle_list(cursor, path, mounts, response);
        }
        [fs_abi::OP_WRITE, a, b, c, d, path_len, rest @ ..]
            if usize::from(*path_len) > 0 && usize::from(*path_len) <= rest.len() =>
        {
            let offset = u32::from_le_bytes([*a, *b, *c, *d]);
            let plen = usize::from(*path_len);
            let path = &rest[..plen];
            let data = &rest[plen..];
            handle_write(offset, path, data, mounts, caps, cache, response);
        }
        _ => {
            response.push(fs_abi::STATUS_ERR);
            response.append(b"sysd: bad request\n");
        }
    }
}

fn main(
    request_handle: u32,
    ext4_handle: u32,
    i2c_system: u32,
    i2c_ext1: u32,
    display_cap: u32,
    sentinel_cap: u32,
) -> ! {
    let caps = Caps {
        dev: dev::DevCaps {
            i2c_system,
            i2c_ext1,
        },
        sys: sys::SysCaps {
            i2c_system,
            display: display_cap,
            sentinel: sentinel_cap,
        },
    };
    let mut mounts = MountTable::new();
    if ext4_handle != 0 {
        let _ = mounts.mount(b"/sd", ext4_handle);
    }
    // SAFETY: single-threaded service, `CACHE` is never aliased.
    let cache = unsafe { &mut *core::ptr::addr_of_mut!(CACHE) };
    let mut request = Buffer::new();
    let mut response = Buffer::new();
    let mut pending_token = 0u32;
    loop {
        let ok = if pending_token != 0 {
            request.reply_and_receive(pending_token, &response, request_handle)
        } else {
            request.receive(request_handle)
        };
        if !ok {
            usys::exit();
        }
        pending_token = request.reply;
        if pending_token == 0 {
            continue;
        }
        handle_request(pending_token, &request, &mounts, caps, cache, &mut response);
    }
}
