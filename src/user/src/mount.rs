//! Userspace VFS mount table, path normalization, and directory listing.
//!
//! Used by `sysd` to route `/sd/*` and dynamically mounted `/dev/*` or `/mnt/*`
//! services to their backing IPC endpoints while serving built-in and synthesized
//! parent directories via `OP_LIST`.

use crate::{Buffer, TextBuf, sys};
use vela_abi::{fs as fs_abi, syscall as abi};

pub const MAX_MOUNTS: usize = 8;
pub const MAX_PREFIX: usize = 32;

#[derive(Clone, Copy)]
struct MountEntry {
    prefix: [u8; MAX_PREFIX],
    prefix_len: usize,
    endpoint: u32,
}

impl MountEntry {
    const EMPTY: Self = Self {
        prefix: [0; MAX_PREFIX],
        prefix_len: 0,
        endpoint: 0,
    };

    fn prefix(&self) -> &[u8] {
        &self.prefix[..self.prefix_len]
    }
}

pub struct MountTable {
    entries: [MountEntry; MAX_MOUNTS],
}

impl Default for MountTable {
    fn default() -> Self {
        Self::new()
    }
}

impl MountTable {
    pub const fn new() -> Self {
        Self {
            entries: [MountEntry::EMPTY; MAX_MOUNTS],
        }
    }

    /// Mount `endpoint` at `prefix` (replacing an existing mount on the same prefix).
    /// Returns `Ok(Some(old_endpoint))` on in-place replace, `Ok(None)` on new slot,
    /// or `Err(())` if `prefix`/`endpoint` is invalid or the mount table is full.
    pub fn mount(&mut self, prefix: &[u8], endpoint: u32) -> Result<Option<u32>, ()> {
        let prefix = normalize_path(prefix);
        if prefix.len() < 2
            || prefix.len() > MAX_PREFIX
            || !prefix.starts_with(b"/")
            || endpoint == 0
        {
            return Err(());
        }
        // Replace an existing mount on the exact same prefix first so a full table
        // can still remount in place.
        for slot in &mut self.entries {
            if slot.endpoint != 0 && slot.prefix() == prefix {
                let old = slot.endpoint;
                slot.endpoint = endpoint;
                return Ok(Some(old));
            }
        }
        for slot in &mut self.entries {
            if slot.endpoint == 0 {
                slot.prefix[..prefix.len()].copy_from_slice(prefix);
                slot.prefix_len = prefix.len();
                slot.endpoint = endpoint;
                return Ok(None);
            }
        }
        Err(())
    }

    /// Unmount `prefix`, returning its previous endpoint handle if present.
    pub fn unmount(&mut self, prefix: &[u8]) -> Option<u32> {
        let prefix = normalize_path(prefix);
        for slot in &mut self.entries {
            if slot.endpoint != 0 && slot.prefix() == prefix {
                let old = slot.endpoint;
                *slot = MountEntry::EMPTY;
                return Some(old);
            }
        }
        None
    }

    pub fn prefix_for_endpoint(&self, endpoint: u32) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|entry| endpoint != 0 && entry.endpoint == endpoint)
            .map(MountEntry::prefix)
    }

    /// Remove any mount backed by `endpoint` (used to auto-prune dead driver endpoints).
    pub fn remove_endpoint(&mut self, endpoint: u32) -> bool {
        if endpoint == 0 {
            return false;
        }
        let mut removed = false;
        for slot in &mut self.entries {
            if slot.endpoint == endpoint {
                *slot = MountEntry::EMPTY;
                removed = true;
            }
        }
        removed
    }

    /// Match the longest mounted prefix for `path`, returning `(endpoint, relative_path)`.
    /// Relative path is always rooted (`b"/"` for exact mount root, `b"/..."` underneath).
    pub fn resolve<'a>(&self, path: &'a [u8]) -> Option<(u32, &'a [u8])> {
        let norm = normalize_path(path);
        let mut best: Option<(usize, u32, &'a [u8])> = None;
        for slot in &self.entries {
            if slot.endpoint == 0 {
                continue;
            }
            let prefix = slot.prefix();
            if norm == prefix {
                if best.is_none_or(|(len, _, _)| prefix.len() > len) {
                    best = Some((prefix.len(), slot.endpoint, b"/"));
                }
            } else if let Some(rest) = norm.strip_prefix(prefix)
                && rest.starts_with(b"/")
                && best.is_none_or(|(len, _, _)| prefix.len() > len)
            {
                best = Some((prefix.len(), slot.endpoint, rest));
            }
        }
        best.map(|(_, ep, rel)| (ep, rel))
    }

    /// True if any active mount lives strictly underneath `dir` (so `dir` is a valid
    /// virtual parent directory, e.g. `/mnt` when `/mnt/ram` is mounted).
    pub fn has_descendant(&self, dir: &[u8]) -> bool {
        let norm = normalize_path(dir);
        self.entries
            .iter()
            .any(|slot| slot.endpoint != 0 && direct_child_under(slot.prefix(), norm).is_some())
    }

    /// Render active mount prefixes (one per line) for `/proc/mounts`.
    pub fn render_mounts<const N: usize>(&self, out: &mut TextBuf<N>) {
        for slot in &self.entries {
            if slot.endpoint != 0 {
                out.append(slot.prefix());
                out.push(b'\n');
            }
        }
    }
}

/// Validate a prefix submitted to `OP_MOUNT` / `OP_UNMOUNT`.
/// Rejects malformed paths and protects core kernel/virtual trees (`/`, `/proc/*`,
/// `/sys/*`, and `/dev` itself). Dynamic mounts under `/dev/<name>`, `/mnt/<name>`,
/// or top-level paths like `/sd` or `/ram` are allowed.
pub fn is_valid_dynamic_prefix(prefix: &[u8]) -> bool {
    let norm = normalize_path(prefix);
    if norm.len() < 2 || norm.len() > MAX_PREFIX || !norm.starts_with(b"/") {
        return false;
    }
    if matches!(norm, b"/proc" | b"/sys" | b"/dev" | b"/sd")
        || norm.starts_with(b"/proc/")
        || norm.starts_with(b"/sys/")
    {
        return false;
    }
    // Check printable ASCII components, no empty segments (`//`), no `.` or `..`.
    for component in norm[1..].split(|&b| b == b'/') {
        if component.is_empty()
            || component == b"."
            || component == b".."
            || !component.iter().all(|&b| (0x21..=0x7e).contains(&b))
        {
            return false;
        }
    }
    true
}

/// Return the immediate child component of `prefix` under `dir`, if `prefix` is strictly
/// beneath `dir` (e.g. `direct_child_under("/dev/ram0", "/dev") == Some("ram0")`,
/// `direct_child_under("/mnt/a/b", "/") == Some("mnt")`).
fn direct_child_under<'a>(prefix: &'a [u8], dir: &[u8]) -> Option<&'a [u8]> {
    let rest = if dir == b"/" {
        prefix.strip_prefix(b"/")?
    } else {
        prefix.strip_prefix(dir)?.strip_prefix(b"/")?
    };
    if rest.is_empty() {
        return None;
    }
    let end = rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Strip trailing `/` bytes while preserving root `"/"`.
pub fn normalize_path(mut path: &[u8]) -> &[u8] {
    while path.len() > 1 && path.ends_with(b"/") {
        path = &path[..path.len() - 1];
    }
    path
}

/// Encode one directory entry for `OP_LIST` from a static name table.
pub fn list_static(entries: &[&[u8]], cursor: u32, response: &mut Buffer) {
    let idx = cursor as usize;
    response.push(fs_abi::STATUS_OK);
    let Some(&name) = entries.get(idx) else {
        return;
    };
    let next = cursor.saturating_add(1);
    response.append(&next.to_le_bytes());
    response.push(1); // complete name fits in one IPC frame
    response.append(name);
}

/// Encode one directory entry for `OP_LIST`, merging `static_entries` with
/// deduplicated direct child names of active mounts under `dir`.
pub fn list_dir(
    static_entries: &[&[u8]],
    dir: &[u8],
    mounts: &MountTable,
    cursor: u32,
    response: &mut Buffer,
) {
    let norm = normalize_path(dir);
    let idx = cursor as usize;
    if idx < static_entries.len() {
        list_static(static_entries, cursor, response);
        return;
    }
    let mut dynamic_idx = static_entries.len();
    for (slot_idx, slot) in mounts.entries.iter().enumerate() {
        if slot.endpoint == 0 {
            continue;
        }
        let Some(child) = direct_child_under(slot.prefix(), norm) else {
            continue;
        };
        if static_entries.contains(&child) {
            continue;
        }
        let dup = mounts.entries[..slot_idx].iter().any(|prev| {
            prev.endpoint != 0 && direct_child_under(prev.prefix(), norm) == Some(child)
        });
        if dup {
            continue;
        }
        if dynamic_idx == idx {
            response.push(fs_abi::STATUS_OK);
            let next = cursor.saturating_add(1);
            response.append(&next.to_le_bytes());
            response.push(1);
            response.append(child);
            return;
        }
        dynamic_idx += 1;
    }
    response.push(fs_abi::STATUS_OK);
}

/// Forward `OP_READ` or `OP_LIST` to a mounted service endpoint with the mount prefix stripped.
/// Returns `false` if the backing endpoint is permanently dead (`-EBADF`/`-EPIPE`).
pub fn forward_pos(
    endpoint: u32,
    op: u8,
    pos: u32,
    rel_path: &[u8],
    response: &mut Buffer,
) -> bool {
    let mut fwd = Buffer::new();
    fwd.push(op);
    fwd.append(&pos.to_le_bytes());
    fwd.append(rel_path);
    forward_raw(endpoint, &fwd, 500, response)
}

/// Forward `OP_WRITE` to a mounted service endpoint with the mount prefix stripped.
/// Returns `false` if the backing endpoint is permanently dead (`-EBADF`/`-EPIPE`).
pub fn forward_write(
    endpoint: u32,
    offset: u32,
    rel_path: &[u8],
    data: &[u8],
    response: &mut Buffer,
) -> bool {
    if rel_path.len() > u8::MAX as usize
        || fs_abi::WRITE_HEADER + rel_path.len() + data.len() > abi::IPC_BYTES
    {
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: write exceeds IPC frame\n");
        return true;
    }
    let mut fwd = Buffer::new();
    fwd.push(fs_abi::OP_WRITE);
    fwd.append(&offset.to_le_bytes());
    fwd.push(rel_path.len() as u8);
    fwd.append(rel_path);
    fwd.append(data);
    forward_raw(endpoint, &fwd, 0, response)
}

/// Forward a single-path operation (`OP_STAT`, `OP_UNLINK`, `OP_MKDIR`) to a
/// mounted service endpoint with the mount prefix stripped.
/// Returns `false` if the backing endpoint is permanently dead (`-EBADF`/`-EPIPE`).
pub fn forward_path_op(endpoint: u32, op: u8, rel_path: &[u8], response: &mut Buffer) -> bool {
    let mut fwd = Buffer::new();
    fwd.push(op);
    fwd.append(rel_path);
    forward_raw(endpoint, &fwd, 500, response)
}

/// Forward `OP_STAT` to a mounted service endpoint with the mount prefix stripped.
/// Returns `false` if the backing endpoint is permanently dead (`-EBADF`/`-EPIPE`).
pub fn forward_stat(endpoint: u32, rel_path: &[u8], response: &mut Buffer) -> bool {
    forward_path_op(endpoint, fs_abi::OP_STAT, rel_path, response)
}

/// Forward `OP_READ_BULK` to a mounted service endpoint, transitively forwarding
/// the caller's bulk memory window bound to `forward_token` with zero intermediate copy.
/// Returns `false` if the backing endpoint is permanently dead (`-EBADF`/`-EPIPE`).
pub fn forward_read_bulk(
    operation: u8,
    endpoint: u32,
    forward_token: u32,
    offset: u32,
    max_len: u32,
    rel_path: &[u8],
    response: &mut Buffer,
) -> bool {
    let mut fwd = Buffer::new();
    fwd.push(operation);
    fwd.append(&offset.to_le_bytes());
    fwd.append(&max_len.to_le_bytes());
    fwd.append(rel_path);
    let desc = abi::IpcBulkCall {
        req_ptr: fwd.ptr() as u32,
        req_len: fwd.len() as u32,
        reply_ptr: response.mut_ptr() as u32,
        reply_cap: abi::IPC_BYTES as u32,
        timeout: 1500,
        bulk_ptr: 0,
        bulk_len: 0,
        bulk_flags: abi::IPC_BULK_FORWARD | abi::IPC_BULK_WRITE,
        forward_token,
    };
    let raw = sys::ipc_call_bulk(endpoint, &desc) as u32;
    let count = raw as usize;
    if !(1..=abi::IPC_BYTES).contains(&count) {
        response.clear();
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: mount unavailable\n");
        return !matches!(raw, abi::EBADF | abi::EPIPE);
    }
    response.set_len(count);
    true
}

/// Forward `OP_WRITE_BULK` to a mounted service endpoint, transitively forwarding
/// the caller's readable bulk memory window bound to `forward_token`.
/// Returns `false` if the backing endpoint is permanently dead (`-EBADF`/`-EPIPE`).
pub fn forward_write_bulk(
    endpoint: u32,
    forward_token: u32,
    offset: u32,
    len: u32,
    rel_path: &[u8],
    response: &mut Buffer,
) -> bool {
    let mut fwd = Buffer::new();
    fwd.push(fs_abi::OP_WRITE_BULK);
    fwd.append(&offset.to_le_bytes());
    fwd.append(&len.to_le_bytes());
    fwd.append(rel_path);
    let desc = abi::IpcBulkCall {
        req_ptr: fwd.ptr() as u32,
        req_len: fwd.len() as u32,
        reply_ptr: response.mut_ptr() as u32,
        reply_cap: abi::IPC_BYTES as u32,
        timeout: 0,
        bulk_ptr: 0,
        bulk_len: 0,
        bulk_flags: abi::IPC_BULK_FORWARD | abi::IPC_BULK_READ,
        forward_token,
    };
    let raw = sys::ipc_call_bulk(endpoint, &desc) as u32;
    let count = raw as usize;
    if !(1..=abi::IPC_BYTES).contains(&count) {
        response.clear();
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: mount unavailable\n");
        return !matches!(raw, abi::EBADF | abi::EPIPE);
    }
    response.set_len(count);
    true
}

fn forward_raw(endpoint: u32, request: &Buffer, timeout: u32, response: &mut Buffer) -> bool {
    let raw = sys::call6(
        abi::IPC_CALL,
        endpoint,
        request.ptr() as u32,
        request.len() as u32,
        response.mut_ptr() as u32,
        abi::IPC_BYTES as u32,
        timeout,
    )
    .0;
    let count = raw as usize;
    if !(1..=abi::IPC_BYTES).contains(&count) {
        response.clear();
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: mount unavailable\n");
        return !matches!(raw, abi::EBADF | abi::EPIPE);
    }
    response.set_len(count);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_trailing_slashes_while_preserving_root() {
        assert_eq!(normalize_path(b"/"), b"/");
        assert_eq!(normalize_path(b"///"), b"/");
        assert_eq!(normalize_path(b"/proc/"), b"/proc");
        assert_eq!(normalize_path(b"/sd///"), b"/sd");
        assert_eq!(normalize_path(b"/sys/sentinel/"), b"/sys/sentinel");
    }

    #[test]
    fn resolves_longest_mount_prefix_with_path_boundaries() {
        let mut mounts = MountTable::new();
        assert_eq!(mounts.mount(b"/sd", 10), Ok(None));
        assert_eq!(mounts.mount(b"/dev/fb1", 20), Ok(None));
        assert_eq!(mounts.mount(b"/sd/sub", 30), Ok(None));

        assert_eq!(mounts.resolve(b"/sd"), Some((10, b"/".as_slice())));
        assert_eq!(mounts.resolve(b"/sd/"), Some((10, b"/".as_slice())));
        assert_eq!(
            mounts.resolve(b"/sd/bin/ls.elf"),
            Some((10, b"/bin/ls.elf".as_slice()))
        );
        // Must not match sibling prefix `/sd2`.
        assert_eq!(mounts.resolve(b"/sd2/foo"), None);
        // Longest prefix wins.
        assert_eq!(
            mounts.resolve(b"/sd/sub/file"),
            Some((30, b"/file".as_slice()))
        );
        assert_eq!(mounts.resolve(b"/dev/fb1"), Some((20, b"/".as_slice())));
        assert_eq!(mounts.resolve(b"/dev/fb10"), None);
    }

    #[test]
    fn mount_rejects_invalid_remounts_in_place_and_unmounts() {
        let mut mounts = MountTable::new();
        assert_eq!(mounts.mount(b"/", 1), Err(()));
        assert_eq!(mounts.mount(b"sd", 1), Err(()));
        assert_eq!(mounts.mount(b"/sd", 0), Err(()));

        assert_eq!(mounts.mount(b"/sd", 1), Ok(None));
        assert_eq!(mounts.mount(b"/sd/", 2), Ok(Some(1)));
        assert_eq!(mounts.resolve(b"/sd"), Some((2, b"/".as_slice())));
        for i in 0..(MAX_MOUNTS - 1) {
            let name = [b'/', b'a' + i as u8];
            assert_eq!(mounts.mount(&name, 10 + i as u32), Ok(None));
        }
        // Full table rejects new mount, but still allows in-place remount and unmount.
        assert_eq!(mounts.mount(b"/overflow", 99), Err(()));
        assert_eq!(mounts.mount(b"/sd", 3), Ok(Some(2)));
        assert_eq!(mounts.unmount(b"/sd"), Some(3));
        assert_eq!(mounts.unmount(b"/sd"), None);
        assert_eq!(mounts.mount(b"/overflow", 99), Ok(None));
        assert!(mounts.remove_endpoint(99));
        assert_eq!(mounts.resolve(b"/overflow"), None);
    }

    #[test]
    fn dynamic_prefix_validation_protects_core_trees() {
        assert!(is_valid_dynamic_prefix(b"/dev/ram0"));
        assert!(is_valid_dynamic_prefix(b"/mnt/usb/"));
        assert!(!is_valid_dynamic_prefix(b"/sd"));
        assert!(!is_valid_dynamic_prefix(b"/sd/"));
        assert!(!is_valid_dynamic_prefix(b"/"));
        assert!(!is_valid_dynamic_prefix(b"/proc"));
        assert!(!is_valid_dynamic_prefix(b"/proc/foo"));
        assert!(!is_valid_dynamic_prefix(b"/sys"));
        assert!(!is_valid_dynamic_prefix(b"/sys/bar"));
        assert!(!is_valid_dynamic_prefix(b"/dev"));
        assert!(!is_valid_dynamic_prefix(b"/dev//foo"));
        assert!(!is_valid_dynamic_prefix(b"/dev/../foo"));
        assert!(!is_valid_dynamic_prefix(b"/dev/bad space"));
    }

    #[test]
    fn list_dir_merges_static_and_deduplicated_dynamic_children() {
        let mut mounts = MountTable::new();
        mounts.mount(b"/sd", 1).unwrap();
        mounts.mount(b"/dev/ram0", 2).unwrap();
        mounts.mount(b"/dev/zero", 3).unwrap(); // overlaps static `/dev/zero`
        mounts.mount(b"/mnt/a", 4).unwrap();
        mounts.mount(b"/mnt/b", 5).unwrap();

        assert!(mounts.has_descendant(b"/mnt"));
        assert!(!mounts.has_descendant(b"/nope"));

        let mut resp = Buffer::new();
        let dev_static: &[&[u8]] = &[b"zero"];
        list_dir(dev_static, b"/dev", &mounts, 0, &mut resp);
        assert_eq!(resp.as_slice(), b"\x00\x01\x00\x00\x00\x01zero");
        resp.clear();
        list_dir(dev_static, b"/dev", &mounts, 1, &mut resp);
        assert_eq!(resp.as_slice(), b"\x00\x02\x00\x00\x00\x01ram0");
        resp.clear();
        list_dir(dev_static, b"/dev", &mounts, 2, &mut resp);
        assert_eq!(resp.as_slice(), &[fs_abi::STATUS_OK]);

        // Root synthesizes `/mnt` once even with two mounts underneath.
        let root_static: &[&[u8]] = &[b"dev", b"proc", b"sd", b"sys"];
        resp.clear();
        list_dir(root_static, b"/", &mounts, 4, &mut resp);
        assert_eq!(resp.as_slice(), b"\x00\x05\x00\x00\x00\x01mnt");
        resp.clear();
        list_dir(root_static, b"/", &mounts, 5, &mut resp);
        assert_eq!(resp.as_slice(), &[fs_abi::STATUS_OK]);
    }

    #[test]
    fn list_static_encodes_entries_and_eof() {
        let entries: &[&[u8]] = &[b"dev", b"proc"];
        let mut response = Buffer::new();

        list_static(entries, 0, &mut response);
        assert_eq!(response.as_slice(), b"\x00\x01\x00\x00\x00\x01dev");

        response.clear();
        list_static(entries, 1, &mut response);
        assert_eq!(response.as_slice(), b"\x00\x02\x00\x00\x00\x01proc");

        response.clear();
        list_static(entries, 2, &mut response);
        assert_eq!(response.as_slice(), &[fs_abi::STATUS_OK]);
    }
}
