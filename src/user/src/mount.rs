//! Userspace VFS mount table, path normalization, and static directory listing.
//!
//! Used by `sysd` to route `/sd/*` (and future dynamically mounted `/dev/*` drivers)
//! to their backing service endpoints while serving built-in directories via `OP_LIST`.

use crate::{Buffer, sys};
use vela_abi::{fs as fs_abi, syscall as abi};

pub const MAX_MOUNTS: usize = 4;
const MAX_PREFIX: usize = 24;

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

    pub fn mount(&mut self, prefix: &[u8], endpoint: u32) -> bool {
        let prefix = normalize_path(prefix);
        if prefix.len() < 2
            || prefix.len() > MAX_PREFIX
            || !prefix.starts_with(b"/")
            || endpoint == 0
        {
            return false;
        }
        for slot in &mut self.entries {
            if slot.endpoint == 0 || slot.prefix() == prefix {
                slot.prefix[..prefix.len()].copy_from_slice(prefix);
                slot.prefix_len = prefix.len();
                slot.endpoint = endpoint;
                return true;
            }
        }
        false
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

/// Forward `OP_READ` or `OP_LIST` to a mounted service endpoint with the mount prefix stripped.
pub fn forward_pos(endpoint: u32, op: u8, pos: u32, rel_path: &[u8], response: &mut Buffer) {
    let mut fwd = Buffer::new();
    fwd.push(op);
    fwd.append(&pos.to_le_bytes());
    fwd.append(rel_path);
    forward_raw(endpoint, &fwd, 500, response);
}

/// Forward `OP_WRITE` to a mounted service endpoint with the mount prefix stripped.
pub fn forward_write(
    endpoint: u32,
    offset: u32,
    rel_path: &[u8],
    data: &[u8],
    response: &mut Buffer,
) {
    if rel_path.len() > u8::MAX as usize
        || fs_abi::WRITE_HEADER + rel_path.len() + data.len() > abi::IPC_BYTES
    {
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: write exceeds IPC frame\n");
        return;
    }
    let mut fwd = Buffer::new();
    fwd.push(fs_abi::OP_WRITE);
    fwd.append(&offset.to_le_bytes());
    fwd.push(rel_path.len() as u8);
    fwd.append(rel_path);
    fwd.append(data);
    forward_raw(endpoint, &fwd, 0, response);
}

fn forward_raw(endpoint: u32, request: &Buffer, timeout: u32, response: &mut Buffer) {
    let count = sys::call6(
        abi::IPC_CALL,
        endpoint,
        request.ptr() as u32,
        request.len() as u32,
        response.mut_ptr() as u32,
        abi::IPC_BYTES as u32,
        timeout,
    )
    .0 as usize;
    if !(1..=abi::IPC_BYTES).contains(&count) {
        response.clear();
        response.push(fs_abi::STATUS_ERR);
        response.append(b"sysd: mount unavailable\n");
        return;
    }
    response.set_len(count);
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
        assert!(mounts.mount(b"/sd", 10));
        assert!(mounts.mount(b"/dev/fb1", 20));
        assert!(mounts.mount(b"/sd/sub", 30));

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
    fn mount_rejects_invalid_and_remounts_in_place() {
        let mut mounts = MountTable::new();
        assert!(!mounts.mount(b"/", 1));
        assert!(!mounts.mount(b"sd", 1));
        assert!(!mounts.mount(b"/sd", 0));

        assert!(mounts.mount(b"/sd", 1));
        assert!(mounts.mount(b"/sd/", 2));
        assert_eq!(mounts.resolve(b"/sd"), Some((2, b"/".as_slice())));
        assert!(mounts.mount(b"/a", 3));
        assert!(mounts.mount(b"/b", 4));
        assert!(mounts.mount(b"/c", 5));
        assert!(!mounts.mount(b"/d", 6));
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
