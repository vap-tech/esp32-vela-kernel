//! Read-only ext4 over partition-relative 512-byte sectors.
//! On-disk structures: https://docs.kernel.org/filesystems/ext4/
pub const SECTOR: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Io,
    Corrupt,
    Unsupported,
    NotFound,
    NotFile,
    NotDirectory,
}

pub trait BlockRead {
    fn sectors(&self) -> u32;
    fn read_sector(&mut self, sector: u32, data: &mut [u8; SECTOR]) -> Result<(), Error>;
}

#[derive(Clone, Copy)]
struct Inode {
    kind: u16,
    size: u32,
    extents: [u8; 60],
}

struct DirEntry {
    id: u32,
    name_at: u64,
    name_len: usize,
    next: u32,
}

pub struct Directory(Inode);

pub struct NameChunk {
    pub next: u32,
    pub len: usize,
    pub end_of_name: bool,
}

pub struct Ext4<B> {
    device: B,
    cached_sector: Option<u32>,
    cache: [u8; SECTOR],
    block_size: u32,
    blocks: u32,
    inodes: u32,
    per_group: u32,
    inode_size: u16,
    desc_size: u16,
    descriptors: u64,
}

fn le16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

impl<B: BlockRead> Ext4<B> {
    pub fn mount(device: B) -> Result<Self, Error> {
        let mut fs = Self {
            device,
            cached_sector: None,
            cache: [0; SECTOR],
            block_size: 0,
            blocks: 0,
            inodes: 0,
            per_group: 0,
            inode_size: 0,
            desc_size: 0,
            descriptors: 0,
        };
        let mut sb = [0; 512];
        fs.read_bytes(1024, &mut sb)?;
        if le16(&sb, 0x38) != 0xef53 {
            return Err(Error::Corrupt);
        }
        let incompat = le32(&sb, 0x60);
        // FILETYPE and EXTENTS required. Allow 64BIT descriptor layout and FLEX_BG.
        // Recovery, metadata checksums, inline data, encryption and bigalloc are unsupported.
        if le16(&sb, 0x3a) != 1
            || le32(&sb, 0x4c) != 1
            || le32(&sb, 0xe8) != 0 // legacy orphan list
            || incompat & 0x42 != 0x42
            || incompat & !0x2c2 != 0
            || le32(&sb, 0x64) & !0x6b != 0
            || le32(&sb, 0x5c) & !0x3f != 0
            || le32(&sb, 0x150) != 0
            || le32(&sb, 0x18) > 2
        {
            return Err(Error::Unsupported);
        }
        fs.block_size = 1024 << le32(&sb, 0x18);
        fs.blocks = le32(&sb, 4);
        fs.inodes = le32(&sb, 0);
        fs.per_group = le32(&sb, 0x28);
        fs.inode_size = le16(&sb, 0x58);
        fs.desc_size = if incompat & 0x80 != 0 {
            le16(&sb, 0xfe)
        } else {
            32
        };
        let first = le32(&sb, 0x14);
        let blocks_per_group = le32(&sb, 0x20);
        if first != u32::from(fs.block_size == 1024)
            || fs.blocks <= first + 1
            || u64::from(fs.blocks) * u64::from(fs.block_size) > fs.capacity()
            || fs.inodes < 2
            || fs.per_group == 0
            || blocks_per_group == 0
            || fs.inodes.div_ceil(fs.per_group) > (fs.blocks - first).div_ceil(blocks_per_group)
            || !(128..=fs.block_size).contains(&u32::from(fs.inode_size))
            || !fs.inode_size.is_power_of_two()
            || !matches!(fs.desc_size, 32 | 64)
        {
            return Err(Error::Corrupt);
        }
        fs.descriptors = fs.block_address(first + 1)?;
        if fs.inode(2)?.kind != 0x4000 {
            return Err(Error::Corrupt);
        }
        Ok(fs)
    }

    fn capacity(&self) -> u64 {
        u64::from(self.device.sectors()) * SECTOR as u64
    }

    fn read_bytes(&mut self, mut offset: u64, mut out: &mut [u8]) -> Result<(), Error> {
        let limit = if self.blocks == 0 {
            self.capacity()
        } else {
            u64::from(self.blocks) * u64::from(self.block_size)
        };
        if offset > limit || out.len() as u64 > limit - offset {
            return Err(Error::Corrupt);
        }
        while !out.is_empty() {
            let sector = (offset / SECTOR as u64) as u32;
            let start = offset as usize % SECTOR;
            if self.cached_sector != Some(sector) {
                self.cached_sector = None;
                self.device.read_sector(sector, &mut self.cache)?;
                self.cached_sector = Some(sector);
            }
            let count = out.len().min(SECTOR - start);
            out[..count].copy_from_slice(&self.cache[start..start + count]);
            out = &mut out[count..];
            offset += count as u64;
        }
        Ok(())
    }

    fn block_address(&self, block: u32) -> Result<u64, Error> {
        if block == 0 || block >= self.blocks {
            return Err(Error::Corrupt);
        }
        Ok(u64::from(block) * u64::from(self.block_size))
    }

    fn inode(&mut self, id: u32) -> Result<Inode, Error> {
        if id == 0 || id > self.inodes {
            return Err(Error::Corrupt);
        }
        let group = (id - 1) / self.per_group;
        let index = (id - 1) % self.per_group;
        let mut desc = [0; 64];
        let size = usize::from(self.desc_size);
        self.read_bytes(
            self.descriptors + u64::from(group) * size as u64,
            &mut desc[..size],
        )?;
        if le32(&desc, 40) != 0 {
            return Err(Error::Unsupported);
        }
        let table = self.block_address(le32(&desc, 8))?;
        let mut raw = [0; 128];
        self.read_bytes(
            table + u64::from(index) * u64::from(self.inode_size),
            &mut raw,
        )?;
        let kind = le16(&raw, 0) & 0xf000;
        if !matches!(kind, 0x4000 | 0x8000) {
            return Err(Error::NotFile);
        }
        let flags = le32(&raw, 32);
        if flags & 0x80000 == 0
            || flags & (0x4 | 0x800 | 0x10000000 | 0x40000000) != 0
            || (kind == 0x4000 && flags & 0x1000 != 0)
            || le32(&raw, 108) != 0
        {
            return Err(Error::Unsupported);
        }
        Ok(Inode {
            kind,
            size: le32(&raw, 4),
            extents: raw[40..100].try_into().unwrap(),
        })
    }

    fn data_block(&mut self, inode: &Inode, logical: u32) -> Result<Option<u64>, Error> {
        let mut header: [u8; 12] = inode.extents[..12].try_into().unwrap();
        let mut node = None;
        let mut depth = le16(&header, 6);
        if depth > 5 {
            return Err(Error::Corrupt);
        }
        loop {
            let entries = usize::from(le16(&header, 2));
            let max = usize::from(le16(&header, 4));
            let capacity = node.map_or(4, |_| (self.block_size as usize - 12) / 12);
            if le16(&header, 0) != 0xf30a
                || le16(&header, 6) != depth
                || max > capacity
                || entries > max
                || (depth != 0 && entries == 0)
            {
                return Err(Error::Corrupt);
            }
            let mut selected = None;
            let mut previous = None;
            for i in 0..entries {
                let offset = 12 + 12 * i;
                let mut entry = [0; 12];
                if let Some(base) = node {
                    self.read_bytes(base + offset as u64, &mut entry)?;
                } else {
                    entry.copy_from_slice(&inode.extents[offset..offset + 12]);
                }
                let start = le32(&entry, 0);
                if previous.is_some_and(|prev| start <= prev) {
                    return Err(Error::Corrupt);
                }
                previous = Some(start);
                if start > logical {
                    break;
                }
                selected = Some(entry);
            }
            let Some(entry) = selected else {
                return Ok(None);
            };
            if depth != 0 {
                if le16(&entry, 8) != 0 {
                    return Err(Error::Unsupported);
                }
                let base = self.block_address(le32(&entry, 4))?;
                self.read_bytes(base, &mut header)?;
                node = Some(base);
                depth -= 1;
            } else {
                let len = u32::from(le16(&entry, 4));
                let delta = logical - le32(&entry, 0);
                if len == 0 || le16(&entry, 6) != 0 {
                    return Err(Error::Corrupt);
                }
                if len > 32768 || delta >= len {
                    return Ok(None);
                }
                let first = le32(&entry, 8);
                if first == 0 || len > self.blocks.saturating_sub(first) {
                    return Err(Error::Corrupt);
                }
                return self.block_address(first + delta).map(Some);
            }
        }
    }

    fn check_directory(&self, dir: &Inode) -> Result<(), Error> {
        if dir.kind != 0x4000 {
            return Err(Error::NotDirectory);
        }
        // Bound the work of a single request, even on a malformed volume.
        if dir.size > 64 * 1024 || dir.size == 0 || !dir.size.is_multiple_of(self.block_size) {
            return Err(Error::Unsupported);
        }
        Ok(())
    }

    fn dir_entry(&mut self, dir: &Inode, offset: u32) -> Result<DirEntry, Error> {
        let remaining = self.block_size - offset % self.block_size;
        if remaining < 8 || !offset.is_multiple_of(4) {
            return Err(Error::Corrupt);
        }
        let base = self
            .data_block(dir, offset / self.block_size)?
            .ok_or(Error::Corrupt)?;
        let addr = base + u64::from(offset % self.block_size);
        let mut header = [0; 8];
        self.read_bytes(addr, &mut header)?;
        let len = u32::from(le16(&header, 4));
        let name_len = u32::from(header[6]);
        let id = le32(&header, 0);
        if len < 8 + name_len
            || len % 4 != 0
            || len > remaining
            || (id != 0 && (name_len == 0 || id > self.inodes))
        {
            return Err(Error::Corrupt);
        }
        Ok(DirEntry {
            id,
            name_at: addr + 8,
            name_len: name_len as usize,
            next: offset + len,
        })
    }

    fn lookup(&mut self, dir: &Inode, name: &[u8]) -> Result<Inode, Error> {
        self.check_directory(dir)?;
        let mut offset = 0;
        let mut buf = [0; 255];
        while offset < dir.size {
            let entry = self.dir_entry(dir, offset)?;
            if entry.id != 0 && entry.name_len == name.len() {
                self.read_bytes(entry.name_at, &mut buf[..name.len()])?;
                if &buf[..name.len()] == name {
                    return self.inode(entry.id);
                }
            }
            offset = entry.next;
        }
        Err(Error::NotFound)
    }

    fn resolve(&mut self, path: &[u8]) -> Result<Inode, Error> {
        let path = path.strip_prefix(b"/").ok_or(Error::NotFound)?;
        let mut inode = self.inode(2)?;
        if path.is_empty() {
            return Ok(inode);
        }
        for name in path.split(|b| *b == b'/') {
            if name.is_empty()
                || name.len() > 255
                || matches!(name, b"." | b"..")
                || name.contains(&0)
            {
                return Err(Error::NotFound);
            }
            inode = self.lookup(&inode, name)?;
        }
        Ok(inode)
    }

    pub fn open(&mut self, path: &[u8]) -> Result<File, Error> {
        let inode = self.resolve(path)?;
        if inode.kind != 0x8000 {
            return Err(Error::NotFile);
        }
        Ok(File(inode))
    }

    pub fn open_dir(&mut self, path: &[u8]) -> Result<Directory, Error> {
        let path = if path.len() > 1 {
            path.strip_suffix(b"/").unwrap_or(path)
        } else {
            path
        };
        let inode = self.resolve(path)?;
        self.check_directory(&inode)?;
        Ok(Directory(inode))
    }

    /// Continue reading names from an opaque cursor. At most one name (or part
    /// of it) is returned per call so even 255-byte names fit the IPC protocol.
    pub fn read_dir(
        &mut self,
        dir: &Directory,
        cursor: u32,
        out: &mut [u8],
    ) -> Result<Option<NameChunk>, Error> {
        // High bits select the directory record, low byte selects a position
        // within its name. A completed name advances to the next record.
        let mut offset = cursor >> 8;
        let consumed = (cursor & 255) as usize;
        if offset > dir.0.size || (offset == dir.0.size && consumed != 0) || out.is_empty() {
            return Err(Error::Corrupt);
        }
        while offset < dir.0.size {
            let entry = self.dir_entry(&dir.0, offset)?;
            if entry.id == 0 || entry.name_len <= 2 {
                let mut name = [0; 2];
                if entry.id != 0 {
                    self.read_bytes(entry.name_at, &mut name[..entry.name_len])?;
                }
                if entry.id == 0 || matches!(&name[..entry.name_len], b"." | b"..") {
                    if consumed != 0 {
                        return Err(Error::Corrupt);
                    }
                    offset = entry.next;
                    continue;
                }
            }
            if consumed >= entry.name_len {
                return Err(Error::Corrupt);
            }
            let len = (entry.name_len - consumed).min(out.len());
            self.read_bytes(entry.name_at + consumed as u64, &mut out[..len])?;
            let end_of_name = consumed + len == entry.name_len;
            let next = if end_of_name {
                entry.next << 8
            } else {
                (offset << 8) | (consumed + len) as u32
            };
            return Ok(Some(NameChunk {
                next,
                len,
                end_of_name,
            }));
        }
        Ok(None)
    }

    pub fn read(&mut self, file: &File, offset: u32, out: &mut [u8]) -> Result<usize, Error> {
        let len = out.len().min(file.0.size.saturating_sub(offset) as usize);
        let mut done = 0;
        while done < len {
            let pos = offset + done as u32;
            let within = pos % self.block_size;
            let count = (len - done).min((self.block_size - within) as usize);
            let chunk = &mut out[done..done + count];
            if let Some(base) = self.data_block(&file.0, pos / self.block_size)? {
                self.read_bytes(base + u64::from(within), chunk)?;
            } else {
                chunk.fill(0);
            }
            done += count;
        }
        Ok(done)
    }
}

pub struct File(Inode);

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command, vec::Vec};

    struct Image(Vec<u8>);

    impl BlockRead for Image {
        fn sectors(&self) -> u32 {
            (self.0.len() / SECTOR) as u32
        }

        fn read_sector(&mut self, sector: u32, out: &mut [u8; SECTOR]) -> Result<(), Error> {
            let start = sector as usize * SECTOR;
            out.copy_from_slice(self.0.get(start..start + SECTOR).ok_or(Error::Io)?);
            Ok(())
        }
    }

    #[test]
    #[ignore = "requires e2fsprogs (mkfs.ext4)"]
    fn reads_real_ext4_images() {
        let root = std::env::temp_dir().join(std::format!("vela-ext4-{}", std::process::id()));
        let files = root.join("files");
        fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        fs::create_dir_all(files.join("bin")).unwrap();
        fs::create_dir(files.join("empty")).unwrap();
        let content: Vec<u8> = (0..14000).map(|i| (i % 251) as u8 + 1).collect();
        fs::write(files.join("bin/large"), &content).unwrap();
        let long_name = "x".repeat(255);
        fs::write(files.join("bin").join(&long_name), b"").unwrap();
        fs::File::create(files.join("sparse"))
            .unwrap()
            .set_len(8192)
            .unwrap();
        for (block, inode) in [("1024", "128"), ("2048", "256"), ("4096", "256")] {
            let path = root.join("fs.img");
            fs::File::create(&path)
                .unwrap()
                .set_len(16 * 1024 * 1024)
                .unwrap();
            let output = Command::new("mkfs.ext4")
                .args([
                    "-F",
                    "-q",
                    "-b",
                    block,
                    "-I",
                    inode,
                    "-O",
                    "^metadata_csum,^metadata_csum_seed,^orphan_file",
                ])
                .arg("-d")
                .arg(&files)
                .arg(&path)
                .output()
                .unwrap();
            assert!(output.status.success(), "{:?}", output);
            let mut bytes = fs::read(&path).unwrap();
            let mut image = Ext4::mount(Image(bytes.clone())).unwrap();
            let file = image.open(b"/bin/large").unwrap();
            assert_eq!(image.open(b"/bin/missing").err(), Some(Error::NotFound));
            let mut buffer = [0; 95];
            for (i, chunk) in content.chunks(buffer.len()).enumerate() {
                let count = image
                    .read(&file, (i * buffer.len()) as u32, &mut buffer)
                    .unwrap();
                assert_eq!(&buffer[..count], chunk);
            }
            assert_eq!(image.read(&file, content.len() as u32, &mut buffer), Ok(0));
            let sparse = image.open(b"/sparse").unwrap();
            assert_eq!(image.read(&sparse, 4064, &mut buffer), Ok(95));
            assert_eq!(buffer, [0; 95]);
            let dir = image.open_dir(b"/bin/").unwrap();
            let mut cursor = 0;
            let mut names = Vec::new();
            let mut name = Vec::new();
            while let Some(chunk) = image.read_dir(&dir, cursor, &mut buffer).unwrap() {
                assert!(chunk.next > cursor);
                name.extend_from_slice(&buffer[..chunk.len]);
                if chunk.end_of_name {
                    names.push(core::mem::take(&mut name));
                }
                cursor = chunk.next;
            }
            names.sort();
            assert_eq!(names, [b"large".to_vec(), long_name.as_bytes().to_vec()]);
            assert!(image.open_dir(b"/").is_ok());
            let empty = image.open_dir(b"/empty").unwrap();
            assert!(image.read_dir(&empty, 0, &mut buffer).unwrap().is_none());
            assert_eq!(
                image.open_dir(b"/bin/large").err(),
                Some(Error::NotDirectory)
            );
            assert_eq!(image.open_dir(b"/missing").err(), Some(Error::NotFound));
            assert_eq!(
                image.read_dir(&dir, 1, &mut buffer).err(),
                Some(Error::Corrupt)
            );
            // INCOMPAT_RECOVER: a read-only mount must not use stale metadata.
            bytes[1024 + 0x60] |= 4;
            assert_eq!(Ext4::mount(Image(bytes)).err(), Some(Error::Unsupported));
        }
    }
}
