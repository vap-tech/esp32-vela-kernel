#![no_std]
#![no_main]
//! `sysd`: root userspace namespace service for `/proc`, `/sys`, `/dev`, and mounted
//! filesystems/drivers (`/sd`, dynamic `/dev/*`, `/mnt/*`).

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

    fn render_or_reuse(
        &mut self,
        offset: usize,
        path: &[u8],
        mounts: &MountTable,
        caps: Caps,
    ) -> (u8, &[u8]) {
        let same_path =
            offset > 0 && path.len() == self.path_len && self.path[..self.path_len] == *path;
        if !same_path {
            self.text.clear();
            self.status = render_virtual(path, mounts, caps, &mut self.text);
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

fn is_virtual_dir(norm: &[u8], mounts: &MountTable) -> bool {
    matches!(norm, b"/" | b"/proc" | b"/sys" | b"/dev" | b"/sys/sentinel")
        || mounts.has_descendant(norm)
}

#[inline(never)]
fn render_virtual(path: &[u8], mounts: &MountTable, caps: Caps, out: &mut TextBuf<2048>) -> u8 {
    let norm = mount::normalize_path(path);
    if norm == b"/proc/mounts" {
        mounts.render_mounts(out);
        return fs_abi::STATUS_OK;
    }
    if let Some(rel) = norm.strip_prefix(b"/proc/") {
        return proc::read(rel, out);
    }
    if let Some(rel) = norm.strip_prefix(b"/sys/") {
        return sys::read(rel, caps.sys, out);
    }
    if let Some(rel) = norm.strip_prefix(b"/dev/") {
        return dev::read(rel, caps.dev, out);
    }
    if is_virtual_dir(norm, mounts) {
        out.append(b"sysd: is a directory\n");
        return fs_abi::STATUS_ERR;
    }
    out.append(b"sysd: no such file\n");
    fs_abi::STATUS_NOENT
}

fn save_dynamic(prefix: &[u8], endpoint: u32) -> bool {
    let key = mount::normalize_path(prefix);
    usys::call(
        abi::SYSD_SAVE_ENDPOINT,
        endpoint,
        key.as_ptr() as u32,
        key.len() as u32,
        0,
    ) == 0
}

fn restore_dynamic(mounts: &mut MountTable) {
    const _: () = assert!(
        mount::MAX_PREFIX == abi::SYSD_KEY_BYTES
            && mount::MAX_MOUNTS == abi::SYSD_RECOVERY_SLOTS + 1
    );
    for slot in 0..abi::SYSD_RECOVERY_SLOTS {
        let mut key = [0; abi::SYSD_KEY_BYTES];
        let ep = usys::call(
            abi::SYSD_LOAD_ENDPOINT,
            slot as u32,
            key.as_mut_ptr() as u32,
            0,
            0,
        );
        if ep == 0 {
            continue;
        }
        if (ep as i32) < 0 {
            usys::exit();
        }
        let len = key.iter().position(|&b| b == 0).unwrap_or(key.len());
        if mounts.mount(&key[..len], ep).is_err() {
            usys::exit();
        }
    }
}

fn prune_if_dead(alive: bool, ep: u32, mounts: &mut MountTable, cache: &mut SnapshotCache) {
    // Keep the reserved slot available for replacement even when all dynamic mounts are used.
    if !alive && !mounts.resolve(b"/sd").is_some_and(|(sd, _)| sd == ep) {
        if let Some(prefix) = mounts.prefix_for_endpoint(ep) {
            save_dynamic(prefix, 0);
        }
        mounts.remove_endpoint(ep);
        let _ = usys::endpoint_close(ep);
        cache.invalidate();
    }
}

fn handle_stat(
    path: &[u8],
    mounts: &mut MountTable,
    caps: Caps,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    if let Some((ep, rel)) = mounts.resolve(path) {
        let alive = mount::forward_stat(ep, rel, response);
        prune_if_dead(alive, ep, mounts, cache);
        return;
    }
    let norm = mount::normalize_path(path);
    if is_virtual_dir(norm, mounts) {
        response.push(fs_abi::STATUS_OK);
        response.append(&0u32.to_le_bytes());
        response.push(fs_abi::KIND_DIR);
        return;
    }
    let (status, bytes) = cache.render_or_reuse(0, path, mounts, caps);
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
    mounts: &mut MountTable,
    caps: Caps,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    if let Some((ep, rel)) = mounts.resolve(path) {
        let alive = mount::forward_pos(ep, fs_abi::OP_READ, offset as u32, rel, response);
        prune_if_dead(alive, ep, mounts, cache);
        return;
    }
    let (status, bytes) = cache.render_or_reuse(offset, path, mounts, caps);
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
    operation: u8,
    pending_token: u32,
    offset: usize,
    max_len: usize,
    path: &[u8],
    mounts: &mut MountTable,
    caps: Caps,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    if let Some((ep, rel)) = mounts.resolve(path) {
        let forwarded_at = usys::monotonic_16m();
        let alive = mount::forward_read_bulk(
            operation,
            ep,
            pending_token,
            offset as u32,
            max_len as u32,
            rel,
            response,
        );
        if response.len() == fs_abi::BULK_READ_SERVICE_PROFILE_BYTES
            && response.get(0) == fs_abi::STATUS_OK
        {
            response.append(&(usys::monotonic_16m() - forwarded_at).to_le_bytes());
        }
        prune_if_dead(alive, ep, mounts, cache);
        return;
    }
    if operation != fs_abi::OP_READ_BULK {
        response.push(fs_abi::STATUS_ERR);
        response.append(b"single-sector reads require an ext4 mount");
        return;
    }
    let (status, bytes) = cache.render_or_reuse(offset, path, mounts, caps);
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

fn handle_list(
    cursor: u32,
    path: &[u8],
    mounts: &mut MountTable,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    if let Some((ep, rel)) = mounts.resolve(path) {
        let alive = mount::forward_pos(ep, fs_abi::OP_LIST, cursor, rel, response);
        prune_if_dead(alive, ep, mounts, cache);
        return;
    }
    match mount::normalize_path(path) {
        b"/" => mount::list_dir(ROOT_ENTRIES, b"/", mounts, cursor, response),
        b"/proc" => mount::list_static(proc::ENTRIES, cursor, response),
        b"/sys" => mount::list_static(sys::ENTRIES, cursor, response),
        b"/sys/sentinel" => mount::list_static(sys::SENTINEL_ENTRIES, cursor, response),
        b"/dev" => mount::list_dir(dev::ENTRIES, b"/dev", mounts, cursor, response),
        other if mounts.has_descendant(other) => {
            mount::list_dir(&[], other, mounts, cursor, response);
        }
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
    mounts: &mut MountTable,
    caps: Caps,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    cache.invalidate();
    if let Some((ep, rel)) = mounts.resolve(path) {
        let alive = mount::forward_write(ep, offset, rel, data, response);
        prune_if_dead(alive, ep, mounts, cache);
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

fn handle_write_bulk(
    pending_token: u32,
    offset: u32,
    len: u32,
    path: &[u8],
    mounts: &mut MountTable,
    caps: Caps,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    cache.invalidate();
    if let Some((ep, rel)) = mounts.resolve(path) {
        let alive = mount::forward_write_bulk(ep, pending_token, offset, len, rel, response);
        prune_if_dead(alive, ep, mounts, cache);
        return;
    }
    let want = len as usize;
    if want > abi::IPC_BYTES {
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: virtual device write too large\n");
        return;
    }
    let mut scratch = [0u8; abi::IPC_BYTES];
    if want > 0 && usys::ipc_bulk_read(pending_token, 0, &mut scratch[..want]).is_err() {
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: bulk read failed\n");
        return;
    }
    let mut inner = Buffer::new();
    handle_write(
        offset,
        path,
        &scratch[..want],
        mounts,
        caps,
        cache,
        &mut inner,
    );
    if inner.get(0) == fs_abi::STATUS_OK {
        response.push(fs_abi::STATUS_OK);
        response.append(&(want as u32).to_le_bytes());
    } else {
        response.append(inner.as_slice());
    }
}

fn handle_mount(
    pending_token: u32,
    prefix: &[u8],
    mounts: &mut MountTable,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    let Ok((rights, has_cap)) = usys::ipc_call_rights(pending_token) else {
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: mount call invalid\n");
        return;
    };
    let allowed = if prefix == b"/sd" {
        rights & abi::CAP_MOUNT_SD != 0
    } else {
        rights & abi::CAP_ADMIN != 0 && mount::is_valid_dynamic_prefix(prefix)
    };
    if !allowed || !has_cap {
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: mount denied or missing endpoint\n");
        return;
    }
    let Ok((new_ep, _)) = usys::ipc_accept_cap(pending_token) else {
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: cannot accept mount endpoint\n");
        return;
    };
    let Ok(previous) = mounts.mount(prefix, new_ep) else {
        let _ = usys::endpoint_close(new_ep);
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: mount table full\n");
        return;
    };
    if prefix != b"/sd" && !save_dynamic(prefix, new_ep) {
        if let Some(old_ep) = previous {
            let _ = mounts.mount(prefix, old_ep);
        } else {
            mounts.unmount(prefix);
        }
        let _ = usys::endpoint_close(new_ep);
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: cannot retain mount\n");
        return;
    }
    if let Some(old_ep) = previous {
        let _ = usys::endpoint_close(old_ep);
    }
    cache.invalidate();
    response.push(fs_abi::STATUS_OK);
}

fn handle_unmount(
    pending_token: u32,
    prefix: &[u8],
    mounts: &mut MountTable,
    cache: &mut SnapshotCache,
    response: &mut Buffer,
) {
    let Ok((rights, _)) = usys::ipc_call_rights(pending_token) else {
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: unmount call invalid\n");
        return;
    };
    if rights & abi::CAP_ADMIN == 0 {
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: unmount permission denied\n");
        return;
    }
    if !mount::is_valid_dynamic_prefix(prefix) {
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: invalid mount prefix\n");
        return;
    }
    if !save_dynamic(prefix, 0) {
        response.push(fs_abi::STATUS_ERR);
        return;
    }
    if let Some(old_ep) = mounts.unmount(prefix) {
        let _ = usys::endpoint_close(old_ep);
        cache.invalidate();
        response.push(fs_abi::STATUS_OK);
    } else {
        response.push(fs_abi::STATUS_NOENT);
        response.append(b"sysd: not mounted\n");
    }
}

fn handle_request(
    pending_token: u32,
    request: &Buffer,
    mounts: &mut MountTable,
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
        [
            op @ (fs_abi::OP_READ_BULK | fs_abi::OP_READ_BULK_SINGLE | fs_abi::OP_READ_BULK_STAGED),
            a,
            b,
            c,
            d,
            e,
            f,
            g,
            h,
            path @ ..,
        ] if !path.is_empty() => {
            let offset = u32::from_le_bytes([*a, *b, *c, *d]) as usize;
            let max_len = u32::from_le_bytes([*e, *f, *g, *h]) as usize;
            handle_read_bulk(
                *op,
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
            handle_list(cursor, path, mounts, cache, response);
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
        [fs_abi::OP_WRITE_BULK, a, b, c, d, e, f, g, h, path @ ..] if !path.is_empty() => {
            let offset = u32::from_le_bytes([*a, *b, *c, *d]);
            let len = u32::from_le_bytes([*e, *f, *g, *h]);
            handle_write_bulk(
                pending_token,
                offset,
                len,
                path,
                mounts,
                caps,
                cache,
                response,
            );
        }
        [fs_abi::OP_MOUNT, prefix @ ..] if !prefix.is_empty() => {
            handle_mount(pending_token, prefix, mounts, cache, response);
        }
        [fs_abi::OP_UNMOUNT, prefix @ ..] if !prefix.is_empty() => {
            handle_unmount(pending_token, prefix, mounts, cache, response);
        }
        [op @ (fs_abi::OP_UNLINK | fs_abi::OP_MKDIR), path @ ..] if !path.is_empty() => {
            cache.invalidate();
            if let Some((ep, rel)) = mounts.resolve(path) {
                let alive = mount::forward_path_op(ep, *op, rel, response);
                prune_if_dead(alive, ep, mounts, cache);
            } else {
                response.push(fs_abi::STATUS_ERR);
                response.append(b"sysd: read-only filesystem\n");
            }
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
    restore_dynamic(&mut mounts);
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
        handle_request(
            pending_token,
            &request,
            &mut mounts,
            caps,
            cache,
            &mut response,
        );
    }
}
