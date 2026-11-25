//! Minimal read-write ext4 over partition-relative 512-byte sectors.
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
    NoSpace,
    AlreadyExists,
    NotEmpty,
}

pub trait BlockRead {
    fn sectors(&self) -> u32;
    fn read_sector(&mut self, sector: u32, data: &mut [u8; SECTOR]) -> Result<(), Error>;
}

pub trait BlockWrite: BlockRead {
    fn write_sector(&mut self, sector: u32, data: &[u8; SECTOR]) -> Result<(), Error>;
}

#[derive(Clone, Copy)]
struct Inode {
    id: u32,
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
    blocks_per_group: u32,
    first_data_block: u32,
    groups: u32,
    first_ino: u32,
    inode_size: u16,
    extra_isize: u16,
    desc_size: u16,
    is_64bit: bool,
    descriptors: u64,
}

#[derive(Clone, Copy, Default)]
struct LeafExtent {
    logical: u32,
    len: u16,
    start: u32,
}

fn le16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn put_le16(bytes: &mut [u8], offset: usize, val: u16) {
    bytes[offset..offset + 2].copy_from_slice(&val.to_le_bytes());
}

fn put_le32(bytes: &mut [u8], offset: usize, val: u32) {
    bytes[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
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
            blocks_per_group: 0,
            first_data_block: 0,
            groups: 0,
            first_ino: 11,
            inode_size: 0,
            extra_isize: 0,
            desc_size: 0,
            is_64bit: false,
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
        fs.is_64bit = incompat & 0x80 != 0;
        fs.desc_size = if fs.is_64bit { le16(&sb, 0xfe) } else { 32 };
        let first = le32(&sb, 0x14);
        let blocks_per_group = le32(&sb, 0x20);
        let first_ino = le32(&sb, 0x54);
        fs.first_ino = if first_ino >= 11 { first_ino } else { 11 };
        let min_extra = le16(&sb, 0x15a);
        let want_extra = le16(&sb, 0x15c);
        fs.extra_isize = if fs.inode_size > 128 {
            let max_extra = fs.inode_size - 128;
            let chosen = want_extra.max(min_extra).max(32);
            chosen.min(max_extra)
        } else {
            0
        };
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
        fs.first_data_block = first;
        fs.blocks_per_group = blocks_per_group;
        fs.groups = (fs.blocks - first).div_ceil(blocks_per_group);
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
            id,
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

    pub fn stat(&mut self, path: &[u8]) -> Result<(u32, bool), Error> {
        let path = if path.len() > 1 {
            path.strip_suffix(b"/").unwrap_or(path)
        } else {
            path
        };
        let inode = self.resolve(path)?;
        Ok((inode.size, inode.kind == 0x4000))
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

impl<B: BlockWrite> Ext4<B> {
    fn write_bytes(&mut self, mut offset: u64, mut data: &[u8]) -> Result<(), Error> {
        let limit = u64::from(self.blocks) * u64::from(self.block_size);
        if offset > limit || data.len() as u64 > limit - offset {
            return Err(Error::Corrupt);
        }
        while !data.is_empty() {
            let sector = (offset / SECTOR as u64) as u32;
            let start = offset as usize % SECTOR;
            let count = data.len().min(SECTOR - start);
            if count < SECTOR {
                if self.cached_sector != Some(sector) {
                    self.cached_sector = None;
                    self.device.read_sector(sector, &mut self.cache)?;
                    self.cached_sector = Some(sector);
                }
                self.cache[start..start + count].copy_from_slice(&data[..count]);
            } else {
                self.cache.copy_from_slice(&data[..SECTOR]);
                self.cached_sector = Some(sector);
            }
            if let Err(e) = self.device.write_sector(sector, &self.cache) {
                self.cached_sector = None;
                return Err(e);
            }
            data = &data[count..];
            offset += count as u64;
        }
        Ok(())
    }

    fn zero_bytes(&mut self, mut offset: u64, mut len: usize) -> Result<(), Error> {
        let zeros = [0u8; 64];
        while len > 0 {
            let step = len.min(zeros.len());
            self.write_bytes(offset, &zeros[..step])?;
            offset += step as u64;
            len -= step;
        }
        Ok(())
    }

    fn zero_block(&mut self, block: u32) -> Result<(), Error> {
        let base = self.block_address(block)?;
        self.zero_bytes(base, self.block_size as usize)
    }

    fn read_desc(&mut self, group: u32) -> Result<[u8; 64], Error> {
        if group >= self.groups {
            return Err(Error::Corrupt);
        }
        let mut desc = [0u8; 64];
        let size = usize::from(self.desc_size);
        self.read_bytes(
            self.descriptors + u64::from(group) * size as u64,
            &mut desc[..size],
        )?;
        if self.desc_size == 64
            && (le32(&desc, 0x20) != 0 || le32(&desc, 0x24) != 0 || le32(&desc, 0x28) != 0)
        {
            return Err(Error::Unsupported);
        }
        Ok(desc)
    }

    fn write_desc(&mut self, group: u32, desc: &[u8; 64]) -> Result<(), Error> {
        let size = usize::from(self.desc_size);
        self.write_bytes(
            self.descriptors + u64::from(group) * size as u64,
            &desc[..size],
        )
    }

    fn desc_free_blocks(&self, desc: &[u8; 64]) -> u32 {
        let lo = u32::from(le16(desc, 0x0c));
        let hi = if self.desc_size == 64 {
            u32::from(le16(desc, 0x2c))
        } else {
            0
        };
        lo | (hi << 16)
    }

    fn set_desc_free_blocks(&self, desc: &mut [u8; 64], val: u32) {
        put_le16(desc, 0x0c, val as u16);
        if self.desc_size == 64 {
            put_le16(desc, 0x2c, (val >> 16) as u16);
        }
    }

    fn desc_free_inodes(&self, desc: &[u8; 64]) -> u32 {
        let lo = u32::from(le16(desc, 0x0e));
        let hi = if self.desc_size == 64 {
            u32::from(le16(desc, 0x2e))
        } else {
            0
        };
        lo | (hi << 16)
    }

    fn set_desc_free_inodes(&self, desc: &mut [u8; 64], val: u32) {
        put_le16(desc, 0x0e, val as u16);
        if self.desc_size == 64 {
            put_le16(desc, 0x2e, (val >> 16) as u16);
        }
    }

    fn desc_itable_unused(&self, desc: &[u8; 64]) -> u32 {
        let lo = u32::from(le16(desc, 0x1c));
        let hi = if self.desc_size == 64 {
            u32::from(le16(desc, 0x3c))
        } else {
            0
        };
        lo | (hi << 16)
    }

    fn set_desc_itable_unused(&self, desc: &mut [u8; 64], val: u32) {
        put_le16(desc, 0x1c, val as u16);
        if self.desc_size == 64 {
            put_le16(desc, 0x3c, (val >> 16) as u16);
        }
    }

    fn desc_used_dirs(&self, desc: &[u8; 64]) -> u32 {
        let lo = u32::from(le16(desc, 0x10));
        let hi = if self.desc_size == 64 {
            u32::from(le16(desc, 0x30))
        } else {
            0
        };
        lo | (hi << 16)
    }

    fn set_desc_used_dirs(&self, desc: &mut [u8; 64], val: u32) {
        put_le16(desc, 0x10, val as u16);
        if self.desc_size == 64 {
            put_le16(desc, 0x30, (val >> 16) as u16);
        }
    }

    fn adjust_sb_free_blocks(&mut self, delta: i64) -> Result<(), Error> {
        let mut lo_buf = [0u8; 4];
        self.read_bytes(1024 + 0x0c, &mut lo_buf)?;
        let lo = u32::from_le_bytes(lo_buf);
        let hi = if self.is_64bit {
            let mut hi_buf = [0u8; 4];
            self.read_bytes(1024 + 0x158, &mut hi_buf)?;
            u32::from_le_bytes(hi_buf)
        } else {
            0
        };
        let cur = u64::from(lo) | (u64::from(hi) << 32);
        let next = if delta >= 0 {
            cur.checked_add(delta as u64).ok_or(Error::Corrupt)?
        } else {
            cur.checked_sub(delta.unsigned_abs())
                .ok_or(Error::Corrupt)?
        };
        self.write_bytes(1024 + 0x0c, &(next as u32).to_le_bytes())?;
        if self.is_64bit {
            self.write_bytes(1024 + 0x158, &((next >> 32) as u32).to_le_bytes())?;
        }
        Ok(())
    }

    fn adjust_sb_free_inodes(&mut self, delta: i32) -> Result<(), Error> {
        let mut buf = [0u8; 4];
        self.read_bytes(1024 + 0x10, &mut buf)?;
        let cur = u32::from_le_bytes(buf);
        let next = if delta >= 0 {
            cur.checked_add(delta as u32).ok_or(Error::Corrupt)?
        } else {
            cur.checked_sub(delta.unsigned_abs())
                .ok_or(Error::Corrupt)?
        };
        self.write_bytes(1024 + 0x10, &next.to_le_bytes())
    }

    fn group_block_count(&self, group: u32) -> u32 {
        let start = self.first_data_block + group * self.blocks_per_group;
        self.blocks.saturating_sub(start).min(self.blocks_per_group)
    }

    fn claim_block_in_group(&mut self, group: u32, bit: u32) -> Result<bool, Error> {
        let mut desc = self.read_desc(group)?;
        let flags = le16(&desc, 0x12);
        let free = self.desc_free_blocks(&desc);
        if flags & 0x2 != 0 || free == 0 || bit >= self.group_block_count(group) {
            return Ok(false);
        }
        let bitmap = self.block_address(le32(&desc, 0x00))?;
        let byte_addr = bitmap + u64::from(bit / 8);
        let mut b = [0u8; 1];
        self.read_bytes(byte_addr, &mut b)?;
        let mask = 1u8 << (bit % 8);
        if b[0] & mask != 0 {
            return Ok(false);
        }
        b[0] |= mask;
        self.write_bytes(byte_addr, &b)?;
        self.set_desc_free_blocks(&mut desc, free - 1);
        self.write_desc(group, &desc)?;
        self.adjust_sb_free_blocks(-1)?;
        Ok(true)
    }

    fn alloc_block(&mut self, hint: Option<u32>) -> Result<u32, Error> {
        if let Some(target) = hint
            && target >= self.first_data_block
            && target < self.blocks
        {
            let rel = target - self.first_data_block;
            let group = rel / self.blocks_per_group;
            let bit = rel % self.blocks_per_group;
            if self.claim_block_in_group(group, bit)? {
                return Ok(target);
            }
        }
        for group in 0..self.groups {
            let mut desc = self.read_desc(group)?;
            let flags = le16(&desc, 0x12);
            let free = self.desc_free_blocks(&desc);
            if flags & 0x2 != 0 || free == 0 {
                continue;
            }
            let bitmap = self.block_address(le32(&desc, 0x00))?;
            let max_bits = self.group_block_count(group);
            let byte_count = max_bits.div_ceil(8);
            let mut chunk = [0u8; 64];
            let mut byte_idx = 0u32;
            while byte_idx < byte_count {
                let step = (byte_count - byte_idx).min(chunk.len() as u32) as usize;
                self.read_bytes(bitmap + u64::from(byte_idx), &mut chunk[..step])?;
                for (i, &val) in chunk[..step].iter().enumerate() {
                    if val == 0xff {
                        continue;
                    }
                    let bit_in_byte = (!val).trailing_zeros();
                    let bit = (byte_idx + i as u32) * 8 + bit_in_byte;
                    if bit >= max_bits {
                        break;
                    }
                    let new_byte = [val | (1u8 << bit_in_byte)];
                    self.write_bytes(bitmap + u64::from(byte_idx + i as u32), &new_byte)?;
                    self.set_desc_free_blocks(&mut desc, free - 1);
                    self.write_desc(group, &desc)?;
                    self.adjust_sb_free_blocks(-1)?;
                    return Ok(self.first_data_block + group * self.blocks_per_group + bit);
                }
                byte_idx += step as u32;
            }
        }
        Err(Error::NoSpace)
    }

    fn free_block(&mut self, block: u32) -> Result<(), Error> {
        if block < self.first_data_block || block >= self.blocks {
            return Err(Error::Corrupt);
        }
        let rel = block - self.first_data_block;
        let group = rel / self.blocks_per_group;
        let bit = rel % self.blocks_per_group;
        let mut desc = self.read_desc(group)?;
        if le16(&desc, 0x12) & 0x2 != 0 {
            return Err(Error::Corrupt);
        }
        let bitmap = self.block_address(le32(&desc, 0x00))?;
        let byte_addr = bitmap + u64::from(bit / 8);
        let mut b = [0u8; 1];
        self.read_bytes(byte_addr, &mut b)?;
        let mask = 1u8 << (bit % 8);
        if b[0] & mask == 0 {
            return Err(Error::Corrupt);
        }
        b[0] &= !mask;
        self.write_bytes(byte_addr, &b)?;
        let free = self.desc_free_blocks(&desc);
        self.set_desc_free_blocks(&mut desc, free + 1);
        self.write_desc(group, &desc)?;
        self.adjust_sb_free_blocks(1)?;
        Ok(())
    }

    fn inode_addr(&mut self, id: u32) -> Result<u64, Error> {
        if id == 0 || id > self.inodes {
            return Err(Error::Corrupt);
        }
        let group = (id - 1) / self.per_group;
        let index = (id - 1) % self.per_group;
        let desc = self.read_desc(group)?;
        let table = self.block_address(le32(&desc, 0x08))?;
        Ok(table + u64::from(index) * u64::from(self.inode_size))
    }

    fn alloc_inode(&mut self) -> Result<u32, Error> {
        for group in 0..self.groups {
            let mut desc = self.read_desc(group)?;
            let flags = le16(&desc, 0x12);
            let free = self.desc_free_inodes(&desc);
            if flags & 0x1 != 0 || free == 0 {
                continue;
            }
            let bitmap = self.block_address(le32(&desc, 0x04))?;
            let table = self.block_address(le32(&desc, 0x08))?;
            let start_bit = if group == 0 {
                self.first_ino.saturating_sub(1)
            } else {
                0
            };
            let byte_count = self.per_group.div_ceil(8);
            let mut chunk = [0u8; 64];
            let mut byte_idx = start_bit / 8;
            while byte_idx < byte_count {
                let step = (byte_count - byte_idx).min(chunk.len() as u32) as usize;
                self.read_bytes(bitmap + u64::from(byte_idx), &mut chunk[..step])?;
                for (i, &val) in chunk[..step].iter().enumerate() {
                    let cur_byte = byte_idx + i as u32;
                    let mut avail = !val;
                    if cur_byte == start_bit / 8 {
                        avail &= 0xffu8.wrapping_shl(start_bit % 8);
                    }
                    if avail == 0 {
                        continue;
                    }
                    let bit_in_byte = avail.trailing_zeros();
                    let index = cur_byte * 8 + bit_in_byte;
                    if index >= self.per_group {
                        break;
                    }
                    let new_byte = [val | (1u8 << bit_in_byte)];
                    self.write_bytes(bitmap + u64::from(cur_byte), &new_byte)?;

                    let unused = self.desc_itable_unused(&desc);
                    let used = self.per_group.saturating_sub(unused);
                    if index >= used {
                        if flags & 0x4 == 0 {
                            for z in used..=index {
                                let slot_addr = table + u64::from(z) * u64::from(self.inode_size);
                                self.zero_bytes(slot_addr, usize::from(self.inode_size))?;
                            }
                        }
                        self.set_desc_itable_unused(&mut desc, self.per_group - 1 - index);
                    }
                    self.set_desc_free_inodes(&mut desc, free - 1);
                    self.write_desc(group, &desc)?;
                    self.adjust_sb_free_inodes(-1)?;

                    let id = group * self.per_group + index + 1;
                    let slot_addr = table + u64::from(index) * u64::from(self.inode_size);
                    self.zero_bytes(slot_addr, usize::from(self.inode_size))?;
                    return Ok(id);
                }
                byte_idx += step as u32;
            }
        }
        Err(Error::NoSpace)
    }

    fn parse_leaf_extents(&self, extents: &[u8; 60]) -> Result<([LeafExtent; 4], usize), Error> {
        if le16(extents, 0) != 0xf30a {
            return Err(Error::Corrupt);
        }
        if le16(extents, 6) != 0 {
            return Err(Error::Unsupported);
        }
        let entries = usize::from(le16(extents, 2));
        let max = usize::from(le16(extents, 4));
        if entries > 4 || max > 4 || entries > max {
            return Err(Error::Corrupt);
        }
        let mut out = [LeafExtent::default(); 4];
        for (i, dst) in out.iter_mut().take(entries).enumerate() {
            let off = 12 + 12 * i;
            let len = le16(extents, off + 4);
            if len == 0 || len > 32768 || le16(extents, off + 6) != 0 {
                return Err(Error::Unsupported);
            }
            *dst = LeafExtent {
                logical: le32(extents, off),
                len,
                start: le32(extents, off + 8),
            };
        }
        Ok((out, entries))
    }

    fn encode_leaf_extents(list: &[LeafExtent; 4], count: usize) -> [u8; 60] {
        let mut raw = [0u8; 60];
        put_le16(&mut raw, 0, 0xf30a);
        put_le16(&mut raw, 2, count as u16);
        put_le16(&mut raw, 4, 4);
        put_le16(&mut raw, 6, 0);
        for (i, ext) in list.iter().take(count).enumerate() {
            let off = 12 + 12 * i;
            put_le32(&mut raw, off, ext.logical);
            put_le16(&mut raw, off + 4, ext.len);
            put_le16(&mut raw, off + 6, 0);
            put_le32(&mut raw, off + 8, ext.start);
        }
        raw
    }

    fn total_leaf_blocks(list: &[LeafExtent; 4], count: usize) -> u32 {
        list.iter().take(count).map(|e| u32::from(e.len)).sum()
    }

    fn flush_inode(&mut self, inode: &Inode, total_fs_blocks: u32) -> Result<(), Error> {
        let addr = self.inode_addr(inode.id)?;
        let sectors_per_block = self.block_size / SECTOR as u32;
        let blocks_512 = total_fs_blocks
            .checked_mul(sectors_per_block)
            .ok_or(Error::Unsupported)?;
        self.write_bytes(addr + 4, &inode.size.to_le_bytes())?;
        self.write_bytes(addr + 28, &blocks_512.to_le_bytes())?;
        self.write_bytes(addr + 40, &inode.extents)?;
        self.write_bytes(addr + 108, &0u32.to_le_bytes())?;
        Ok(())
    }

    fn free_inode(&mut self, id: u32) -> Result<(), Error> {
        if id < self.first_ino || id > self.inodes {
            return Err(Error::Corrupt);
        }
        let group = (id - 1) / self.per_group;
        let index = (id - 1) % self.per_group;
        let mut desc = self.read_desc(group)?;
        if le16(&desc, 0x12) & 0x1 != 0 {
            return Err(Error::Corrupt);
        }
        let bitmap = self.block_address(le32(&desc, 0x04))?;
        let byte_addr = bitmap + u64::from(index / 8);
        let mut b = [0u8; 1];
        self.read_bytes(byte_addr, &mut b)?;
        let mask = 1u8 << (index % 8);
        if b[0] & mask == 0 {
            return Err(Error::Corrupt);
        }
        b[0] &= !mask;
        self.write_bytes(byte_addr, &b)?;
        let free = self.desc_free_inodes(&desc);
        self.set_desc_free_inodes(&mut desc, free + 1);
        self.write_desc(group, &desc)?;
        self.adjust_sb_free_inodes(1)?;
        Ok(())
    }

    fn adjust_group_used_dirs(&mut self, id: u32, delta: i32) -> Result<(), Error> {
        let group = (id - 1) / self.per_group;
        let mut desc = self.read_desc(group)?;
        let cur = self.desc_used_dirs(&desc);
        let next = if delta >= 0 {
            cur.checked_add(delta as u32).ok_or(Error::Corrupt)?
        } else {
            cur.checked_sub(delta.unsigned_abs())
                .ok_or(Error::Corrupt)?
        };
        self.set_desc_used_dirs(&mut desc, next);
        self.write_desc(group, &desc)
    }

    fn read_inode_links(&mut self, id: u32) -> Result<u16, Error> {
        let addr = self.inode_addr(id)?;
        let mut buf = [0u8; 2];
        self.read_bytes(addr + 26, &mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }

    fn write_inode_links(&mut self, id: u32, links: u16) -> Result<(), Error> {
        let addr = self.inode_addr(id)?;
        self.write_bytes(addr + 26, &links.to_le_bytes())
    }

    fn init_regular_inode(&mut self, id: u32) -> Result<Inode, Error> {
        let addr = self.inode_addr(id)?;
        let extents = Self::encode_leaf_extents(&[LeafExtent::default(); 4], 0);
        let mut raw = [0u8; 128];
        put_le16(&mut raw, 0, 0x81a4); // S_IFREG | 0644
        put_le16(&mut raw, 26, 1); // i_links_count = 1
        put_le32(&mut raw, 32, 0x80000); // EXT4_EXTENTS_FL
        raw[40..100].copy_from_slice(&extents);
        self.write_bytes(addr, &raw)?;
        if self.inode_size > 128 {
            self.write_bytes(addr + 128, &self.extra_isize.to_le_bytes())?;
        }
        Ok(Inode {
            id,
            kind: 0x8000,
            size: 0,
            extents,
        })
    }

    fn ensure_logical_block(
        &mut self,
        list: &mut [LeafExtent; 4],
        count: &mut usize,
        logical: u32,
    ) -> Result<u32, Error> {
        let mut hint = None;
        for ext in list.iter().take(*count) {
            let end = ext.logical + u32::from(ext.len);
            if logical >= ext.logical && logical < end {
                return Ok(ext.start + (logical - ext.logical));
            }
            if end == logical {
                hint = Some(ext.start + u32::from(ext.len));
            }
        }
        let phys = self.alloc_block(hint)?;
        if let Err(e) = self.zero_block(phys) {
            let _ = self.free_block(phys);
            return Err(e);
        }
        // Find sorted insertion index.
        let mut pos = 0;
        while pos < *count && list[pos].logical < logical {
            pos += 1;
        }
        // Coalesce with preceding extent if contiguous.
        if pos > 0
            && list[pos - 1].logical + u32::from(list[pos - 1].len) == logical
            && list[pos - 1].start + u32::from(list[pos - 1].len) == phys
            && list[pos - 1].len < 32768
        {
            list[pos - 1].len += 1;
            // Also merge with following extent if now contiguous.
            if pos < *count {
                let prev = list[pos - 1];
                let next = list[pos];
                let prev_end = prev.logical + u32::from(prev.len);
                let prev_phys_end = prev.start + u32::from(prev.len);
                if prev_end == next.logical
                    && prev_phys_end == next.start
                    && u32::from(prev.len) + u32::from(next.len) <= 32768
                {
                    list[pos - 1].len += next.len;
                    for j in pos..(*count - 1) {
                        list[j] = list[j + 1];
                    }
                    *count -= 1;
                }
            }
            return Ok(phys);
        }
        // Coalesce with following extent if contiguous.
        if pos < *count {
            let next = &mut list[pos];
            if logical + 1 == next.logical && phys + 1 == next.start && next.len < 32768 {
                next.logical = logical;
                next.start = phys;
                next.len += 1;
                return Ok(phys);
            }
        }
        if *count >= 4 {
            let _ = self.free_block(phys);
            return Err(Error::NoSpace);
        }
        for j in (pos..*count).rev() {
            list[j + 1] = list[j];
        }
        list[pos] = LeafExtent {
            logical,
            len: 1,
            start: phys,
        };
        *count += 1;
        Ok(phys)
    }

    fn truncate_extents(
        &mut self,
        list: &mut [LeafExtent; 4],
        count: &mut usize,
        keep_blocks: u32,
    ) -> Result<(), Error> {
        let mut out_idx = 0;
        for i in 0..*count {
            let ext = list[i];
            if ext.logical >= keep_blocks {
                for b in 0..u32::from(ext.len) {
                    self.free_block(ext.start + b)?;
                }
            } else if ext.logical + u32::from(ext.len) > keep_blocks {
                let keep = (keep_blocks - ext.logical) as u16;
                for b in u32::from(keep)..u32::from(ext.len) {
                    self.free_block(ext.start + b)?;
                }
                list[out_idx] = LeafExtent {
                    logical: ext.logical,
                    len: keep,
                    start: ext.start,
                };
                out_idx += 1;
            } else {
                list[out_idx] = ext;
                out_idx += 1;
            }
        }
        *count = out_idx;
        Ok(())
    }

    fn add_dir_entry(
        &mut self,
        dir: &mut Inode,
        name: &[u8],
        child_ino: u32,
        file_type: u8,
    ) -> Result<(), Error> {
        self.check_directory(dir)?;
        let need = ((8 + name.len() + 3) & !3) as u32;
        let mut offset = 0u32;
        while offset < dir.size {
            let entry = self.dir_entry(dir, offset)?;
            let rec_len = entry.next - offset;
            let base = self
                .data_block(dir, offset / self.block_size)?
                .ok_or(Error::Corrupt)?;
            let entry_addr = base + u64::from(offset % self.block_size);
            if entry.id == 0 && rec_len >= need {
                let mut hdr = [0u8; 8];
                put_le32(&mut hdr, 0, child_ino);
                put_le16(&mut hdr, 4, rec_len as u16);
                hdr[6] = name.len() as u8;
                hdr[7] = file_type;
                self.write_bytes(entry_addr, &hdr)?;
                self.write_bytes(entry_addr + 8, name)?;
                return Ok(());
            }
            if entry.id != 0 {
                let actual = ((8 + entry.name_len + 3) & !3) as u32;
                let slack = rec_len.saturating_sub(actual);
                if slack >= need {
                    self.write_bytes(entry_addr + 4, &(actual as u16).to_le_bytes())?;
                    let new_addr = entry_addr + u64::from(actual);
                    let mut hdr = [0u8; 8];
                    put_le32(&mut hdr, 0, child_ino);
                    put_le16(&mut hdr, 4, slack as u16);
                    hdr[6] = name.len() as u8;
                    hdr[7] = file_type;
                    self.write_bytes(new_addr, &hdr)?;
                    self.write_bytes(new_addr + 8, name)?;
                    return Ok(());
                }
            }
            offset = entry.next;
        }
        // Append a new directory block if within the 64 KiB directory bound.
        if dir.size + self.block_size > 64 * 1024 {
            return Err(Error::NoSpace);
        }
        let (mut list, mut count) = self.parse_leaf_extents(&dir.extents)?;
        let logical = dir.size / self.block_size;
        let phys = self.ensure_logical_block(&mut list, &mut count, logical)?;
        let base = self.block_address(phys)?;
        let mut hdr = [0u8; 8];
        put_le32(&mut hdr, 0, child_ino);
        put_le16(&mut hdr, 4, self.block_size as u16);
        hdr[6] = name.len() as u8;
        hdr[7] = file_type;
        self.write_bytes(base, &hdr)?;
        self.write_bytes(base + 8, name)?;
        dir.size += self.block_size;
        dir.extents = Self::encode_leaf_extents(&list, count);
        let total = Self::total_leaf_blocks(&list, count);
        self.flush_inode(dir, total)?;
        Ok(())
    }

    fn resolve_parent<'a>(&mut self, path: &'a [u8]) -> Result<(Inode, &'a [u8]), Error> {
        let trimmed = path.strip_prefix(b"/").ok_or(Error::NotFound)?;
        if trimmed.is_empty() || trimmed.ends_with(b"/") {
            return Err(Error::NotFile);
        }
        let (parent_path, name) = match trimmed.iter().rposition(|&b| b == b'/') {
            Some(pos) => (&trimmed[..pos], &trimmed[pos + 1..]),
            None => (b"".as_slice(), trimmed),
        };
        if name.is_empty() || name.len() > 255 || matches!(name, b"." | b"..") || name.contains(&0)
        {
            return Err(Error::NotFound);
        }
        let mut dir = self.inode(2)?;
        if !parent_path.is_empty() {
            for comp in parent_path.split(|&b| b == b'/') {
                if comp.is_empty()
                    || comp.len() > 255
                    || matches!(comp, b"." | b"..")
                    || comp.contains(&0)
                {
                    return Err(Error::NotFound);
                }
                dir = self.lookup(&dir, comp)?;
            }
        }
        self.check_directory(&dir)?;
        Ok((dir, name))
    }

    /// Write `data` to `path` at `offset`, creating the file in an existing parent
    /// directory if absent. Writing at `offset == 0` truncates an existing file to
    /// `data.len()` and frees any excess blocks.
    pub fn write(&mut self, path: &[u8], offset: u32, data: &[u8]) -> Result<usize, Error> {
        let end = offset
            .checked_add(u32::try_from(data.len()).map_err(|_| Error::Unsupported)?)
            .ok_or(Error::Unsupported)?;
        let (mut parent, name) = self.resolve_parent(path)?;
        let mut inode = match self.lookup(&parent, name) {
            Ok(existing) => {
                if existing.kind != 0x8000 {
                    return Err(Error::NotFile);
                }
                existing
            }
            Err(Error::NotFound) => {
                let id = self.alloc_inode()?;
                let new_inode = self.init_regular_inode(id)?;
                self.add_dir_entry(&mut parent, name, id, 1)?;
                new_inode
            }
            Err(e) => return Err(e),
        };

        let (mut list, mut count) = self.parse_leaf_extents(&inode.extents)?;
        if offset == 0 {
            let keep_blocks = end.div_ceil(self.block_size);
            self.truncate_extents(&mut list, &mut count, keep_blocks)?;
            inode.size = end;
        } else if end > inode.size {
            inode.size = end;
        }

        let mut done = 0usize;
        while done < data.len() {
            let pos = offset + done as u32;
            let logical = pos / self.block_size;
            let within = pos % self.block_size;
            let step = (data.len() - done).min((self.block_size - within) as usize);
            let phys = self.ensure_logical_block(&mut list, &mut count, logical)?;
            let base = self.block_address(phys)?;
            self.write_bytes(base + u64::from(within), &data[done..done + step])?;
            done += step;
        }

        inode.extents = Self::encode_leaf_extents(&list, count);
        let total = Self::total_leaf_blocks(&list, count);
        self.flush_inode(&inode, total)?;
        Ok(done)
    }

    /// Create a new empty directory at `path` inside an existing parent directory.
    pub fn mkdir(&mut self, path: &[u8]) -> Result<(), Error> {
        let path = if path.len() > 1 {
            path.strip_suffix(b"/").unwrap_or(path)
        } else {
            path
        };
        let (mut parent, name) = self.resolve_parent(path)?;
        match self.lookup(&parent, name) {
            Ok(_) => return Err(Error::AlreadyExists),
            Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
        let parent_links = self.read_inode_links(parent.id)?;
        if parent_links >= 65000 {
            return Err(Error::NoSpace);
        }
        let id = self.alloc_inode()?;
        let block = match self.alloc_block(None) {
            Ok(b) => b,
            Err(e) => {
                let _ = self.free_inode(id);
                return Err(e);
            }
        };
        if let Err(e) = self.zero_block(block) {
            let _ = self.free_block(block);
            let _ = self.free_inode(id);
            return Err(e);
        }
        // Populate `.` and `..` directory entries in the new directory block.
        let base = self.block_address(block)?;
        let mut dot = [0u8; 12];
        put_le32(&mut dot, 0, id);
        put_le16(&mut dot, 4, 12);
        dot[6] = 1;
        dot[7] = 2; // EXT4_FT_DIR
        dot[8] = b'.';
        let mut dotdot = [0u8; 12];
        put_le32(&mut dotdot, 0, parent.id);
        put_le16(&mut dotdot, 4, (self.block_size - 12) as u16);
        dotdot[6] = 2;
        dotdot[7] = 2; // EXT4_FT_DIR
        dotdot[8] = b'.';
        dotdot[9] = b'.';
        self.write_bytes(base, &dot)?;
        self.write_bytes(base + 12, &dotdot)?;

        // Write the directory inode.
        let mut list = [LeafExtent::default(); 4];
        list[0] = LeafExtent {
            logical: 0,
            len: 1,
            start: block,
        };
        let extents = Self::encode_leaf_extents(&list, 1);
        let addr = self.inode_addr(id)?;
        let mut raw = [0u8; 128];
        put_le16(&mut raw, 0, 0x41ed); // S_IFDIR | 0755
        put_le32(&mut raw, 4, self.block_size);
        put_le16(&mut raw, 26, 2); // `.` + parent entry
        put_le32(&mut raw, 28, self.block_size / SECTOR as u32);
        put_le32(&mut raw, 32, 0x80000); // EXT4_EXTENTS_FL
        raw[40..100].copy_from_slice(&extents);
        self.write_bytes(addr, &raw)?;
        if self.inode_size > 128 {
            self.write_bytes(addr + 128, &self.extra_isize.to_le_bytes())?;
        }

        if let Err(e) = self.add_dir_entry(&mut parent, name, id, 2) {
            let _ = self.zero_bytes(addr, usize::from(self.inode_size));
            let _ = self.free_block(block);
            let _ = self.free_inode(id);
            return Err(e);
        }
        self.write_inode_links(parent.id, parent_links + 1)?;
        self.adjust_group_used_dirs(id, 1)?;
        Ok(())
    }

    fn is_dir_empty(&mut self, dir: &Inode) -> Result<bool, Error> {
        self.check_directory(dir)?;
        let mut offset = 0u32;
        let mut buf = [0u8; 2];
        while offset < dir.size {
            let entry = self.dir_entry(dir, offset)?;
            if entry.id != 0 {
                if entry.name_len > 2 {
                    return Ok(false);
                }
                self.read_bytes(entry.name_at, &mut buf[..entry.name_len])?;
                if !matches!(&buf[..entry.name_len], b"." | b"..") {
                    return Ok(false);
                }
            }
            offset = entry.next;
        }
        Ok(true)
    }

    /// Remove a regular file or an empty directory at `path`.
    pub fn unlink(&mut self, path: &[u8]) -> Result<(), Error> {
        let path = if path.len() > 1 {
            path.strip_suffix(b"/").unwrap_or(path)
        } else {
            path
        };
        let (parent, name) = self.resolve_parent(path)?;
        let mut offset = 0u32;
        let mut prev_offset: Option<u32> = None;
        let mut buf = [0u8; 255];
        let mut found = None;
        while offset < parent.size {
            if offset % self.block_size == 0 {
                prev_offset = None;
            }
            let entry = self.dir_entry(&parent, offset)?;
            if entry.id != 0 && entry.name_len == name.len() {
                self.read_bytes(entry.name_at, &mut buf[..name.len()])?;
                if &buf[..name.len()] == name {
                    found = Some((prev_offset, offset, entry.next - offset, entry.id));
                    break;
                }
            }
            prev_offset = Some(offset);
            offset = entry.next;
        }
        let Some((prev_off, cur_off, cur_rec_len, target_id)) = found else {
            return Err(Error::NotFound);
        };
        if target_id == 2 {
            return Err(Error::NotEmpty);
        }
        let target = self.inode(target_id)?;
        let is_dir = target.kind == 0x4000;
        if is_dir && !self.is_dir_empty(&target)? {
            return Err(Error::NotEmpty);
        }
        // Validate extent tree before mutating parent directory.
        let (mut list, mut count) = self.parse_leaf_extents(&target.extents)?;

        // Unlink directory entry from parent.
        let block_base = self
            .data_block(&parent, cur_off / self.block_size)?
            .ok_or(Error::Corrupt)?;
        if let Some(prev) = prev_off {
            let prev_addr = block_base + u64::from(prev % self.block_size);
            let mut prev_len_buf = [0u8; 2];
            self.read_bytes(prev_addr + 4, &mut prev_len_buf)?;
            let new_len = u16::from_le_bytes(prev_len_buf)
                .checked_add(cur_rec_len as u16)
                .ok_or(Error::Corrupt)?;
            self.write_bytes(prev_addr + 4, &new_len.to_le_bytes())?;
        } else {
            let cur_addr = block_base + u64::from(cur_off % self.block_size);
            self.write_bytes(cur_addr, &0u32.to_le_bytes())?;
        }

        let links = self.read_inode_links(target_id)?;
        let drop_inode = if is_dir {
            let parent_links = self.read_inode_links(parent.id)?;
            if parent_links > 2 {
                self.write_inode_links(parent.id, parent_links - 1)?;
            }
            self.adjust_group_used_dirs(target_id, -1)?;
            true
        } else if links > 1 {
            self.write_inode_links(target_id, links - 1)?;
            false
        } else {
            true
        };

        if drop_inode {
            self.truncate_extents(&mut list, &mut count, 0)?;
            let addr = self.inode_addr(target_id)?;
            self.zero_bytes(addr, usize::from(self.inode_size))?;
            self.free_inode(target_id)?;
        }
        Ok(())
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

    impl BlockWrite for Image {
        fn write_sector(&mut self, sector: u32, data: &[u8; SECTOR]) -> Result<(), Error> {
            let start = sector as usize * SECTOR;
            self.0
                .get_mut(start..start + SECTOR)
                .ok_or(Error::Io)?
                .copy_from_slice(data);
            Ok(())
        }
    }

    #[test]
    fn writes_and_creates_files_passing_e2fsck() {
        // Run against real mkfs.ext4 + e2fsck when e2fsprogs is present on the host.
        if Command::new("mkfs.ext4").arg("-V").output().is_err()
            || Command::new("e2fsck").arg("-V").output().is_err()
        {
            return;
        }
        let root = std::env::temp_dir().join(std::format!("vela-ext4-rw-{}", std::process::id()));
        let files = root.join("files");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        fs::create_dir_all(files.join("etc")).unwrap();
        let initial: Vec<u8> = (0..10_000).map(|i| (i % 239) as u8 + 1).collect();
        fs::write(files.join("etc/existing.bin"), &initial).unwrap();

        for (block, inode) in [("1024", "128"), ("2048", "256"), ("4096", "256")] {
            let path = root.join(std::format!("rw-{block}-{inode}.img"));
            fs::File::create(&path)
                .unwrap()
                .set_len(16 * 1024 * 1024)
                .unwrap();
            let mkfs = Command::new("mkfs.ext4")
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
            assert!(mkfs.status.success(), "{:?}", mkfs);

            let bytes = fs::read(&path).unwrap();
            let mut fs_img = Ext4::mount(Image(bytes)).unwrap();

            // 1. Create a new small file in root.
            assert_eq!(
                fs_img.write(
                    b"/hello.txt",
                    0,
                    b"hello vela
"
                ),
                Ok(11)
            );
            // 2. Create a multi-block file in /etc via streaming chunks.
            let payload: Vec<u8> = (0..18_000).map(|i| ((i * 7 + 3) % 251) as u8).collect();
            let mut off = 0u32;
            for chunk in payload.chunks(3000) {
                assert_eq!(
                    fs_img.write(b"/etc/stream.dat", off, chunk),
                    Ok(chunk.len())
                );
                off += chunk.len() as u32;
            }
            // 3. Overwrite & shrink existing 10 KiB file to 5 bytes (frees excess blocks).
            assert_eq!(fs_img.write(b"/etc/existing.bin", 0, b"short"), Ok(5));
            // 4. Create an empty 0-byte file.
            assert_eq!(fs_img.write(b"/empty.txt", 0, b""), Ok(0));
            // 5. Reject writing inside a missing directory or onto a directory itself.
            assert_eq!(fs_img.write(b"/nope/a.txt", 0, b"x"), Err(Error::NotFound));
            assert_eq!(fs_img.write(b"/etc", 0, b"x"), Err(Error::NotFile));

            // 6. mkdir + nested subdir + file creation + unlink + empty-dir removal.
            assert_eq!(fs_img.mkdir(b"/tmp"), Ok(()));
            assert_eq!(fs_img.mkdir(b"/tmp"), Err(Error::AlreadyExists));
            assert_eq!(fs_img.mkdir(b"/tmp/sub"), Ok(()));
            assert_eq!(fs_img.write(b"/tmp/sub/note.txt", 0, b"nested\n"), Ok(7));
            assert_eq!(fs_img.write(b"/tmp/drop_me.txt", 0, b"bye"), Ok(3));
            // Non-empty directory cannot be unlinked.
            assert_eq!(fs_img.unlink(b"/tmp/sub"), Err(Error::NotEmpty));
            assert_eq!(fs_img.unlink(b"/tmp/drop_me.txt"), Ok(()));
            assert_eq!(fs_img.unlink(b"/tmp/drop_me.txt"), Err(Error::NotFound));
            assert_eq!(fs_img.unlink(b"/tmp/sub/note.txt"), Ok(()));
            // Now /tmp/sub is empty and can be unlinked cleanly.
            assert_eq!(fs_img.unlink(b"/tmp/sub"), Ok(()));
            // Also keep one newly created dir + file to verify e2fsck on persisted dirs.
            assert_eq!(fs_img.write(b"/tmp/keep.txt", 0, b"kept"), Ok(4));

            // Re-mount cleanly from written bytes and verify contents!
            let written_bytes = fs_img.device.0;
            fs::write(&path, &written_bytes).unwrap();
            let mut check = Ext4::mount(Image(written_bytes)).unwrap();

            let f1 = check.open(b"/hello.txt").unwrap();
            let mut buf = [0u8; 32];
            let n1 = check.read(&f1, 0, &mut buf).unwrap();
            assert_eq!(
                &buf[..n1],
                b"hello vela
"
            );

            let f2 = check.open(b"/etc/stream.dat").unwrap();
            let mut read_back = std::vec![0u8; payload.len()];
            let n2 = check.read(&f2, 0, &mut read_back).unwrap();
            assert_eq!(n2, payload.len());
            assert_eq!(read_back, payload);

            let f3 = check.open(b"/etc/existing.bin").unwrap();
            let n3 = check.read(&f3, 0, &mut buf).unwrap();
            assert_eq!(&buf[..n3], b"short");

            assert_eq!(check.stat(b"/empty.txt"), Ok((0, false)));
            assert_eq!(
                check.stat(b"/tmp"),
                Ok((block.parse::<u32>().unwrap(), true))
            );
            assert_eq!(check.stat(b"/tmp/keep.txt"), Ok((4, false)));
            assert_eq!(check.stat(b"/tmp/sub"), Err(Error::NotFound));
            assert_eq!(check.stat(b"/tmp/drop_me.txt"), Err(Error::NotFound));

            // Run e2fsck -fn to prove 100% on-disk ext4 metadata consistency!
            let fsck = Command::new("e2fsck")
                .args(["-f", "-n"])
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                fsck.status.success(),
                "e2fsck failed for block={block} inode={inode}: stdout={} stderr={}",
                std::string::String::from_utf8_lossy(&fsck.stdout),
                std::string::String::from_utf8_lossy(&fsck.stderr),
            );
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
