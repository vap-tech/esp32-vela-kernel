#![no_std]
#![no_main]

use core::cell::Cell;

use vela_abi::{fs as fs_abi, syscall as abi};
use vela_user::{
    Buffer,
    ext4::{BlockRead, BlockWrite, Directory, Error, Ext4, File, SECTOR},
    sys,
};

vela_user::entry!(main);

/// NOLOAD `.bss` staging buffer for `OP_READ_BULK` (keeps the 4 KiB service stack clear).
#[repr(C, align(64))]
struct BulkChunk([u8; 4096]);

static mut BULK_CHUNK: BulkChunk = BulkChunk([0; 4096]);

struct Partition<'a> {
    profiled: &'a Cell<bool>,
    handle: u32,
    sectors: u32,
}

impl Partition<'_> {
    fn profile_flag(&self) -> u32 {
        if self.profiled.get() {
            abi::IO_PROFILE
        } else {
            0
        }
    }
}

impl BlockRead for Partition<'_> {
    fn sectors(&self) -> u32 {
        self.sectors
    }

    fn read_sectors(&mut self, sector: u32, out: &mut [u8]) -> Result<(), Error> {
        if out.is_empty() || !out.len().is_multiple_of(SECTOR) || out.len() > 8 * SECTOR {
            return Err(Error::Io);
        }
        if sys::call6(
            abi::SD_PARTITION,
            self.handle,
            sector,
            out.as_mut_ptr() as u32,
            abi::SD_PARTITION_READ_BATCH | self.profile_flag(),
            (out.len() / SECTOR) as u32,
            0,
        )
        .0 == 0
        {
            Ok(())
        } else {
            Err(Error::Io)
        }
    }

    fn read_sector(&mut self, sector: u32, out: &mut [u8; SECTOR]) -> Result<(), Error> {
        if sys::call(
            abi::SD_PARTITION,
            self.handle,
            sector,
            out.as_mut_ptr() as u32,
            self.profile_flag(),
        ) == 0
        {
            Ok(())
        } else {
            Err(Error::Io)
        }
    }
}

impl BlockWrite for Partition<'_> {
    fn write_sector(&mut self, sector: u32, data: &[u8; SECTOR]) -> Result<(), Error> {
        if sys::call(
            abi::SD_PARTITION,
            self.handle,
            sector,
            data.as_ptr() as u32,
            abi::SD_PARTITION_WRITE,
        ) == 0
        {
            Ok(())
        } else {
            Err(Error::Io)
        }
    }
}

fn main(endpoint: u32, storage: u32, mount_vfs: u32, _a3: u32, _a4: u32, _a5: u32) -> ! {
    // Single-threaded service: the current RPC selects profiling for every
    // read of its Partition, including metadata. Each request resets the Cell.
    let profiling = Cell::new(false);
    let mut mount = None;
    let mut mounted_epoch = 0;
    let mut opened: Option<(Buffer, File)> = None;
    let mut directory: Option<(Buffer, Directory)> = None;
    let mut request = Buffer::new();
    let mut response = Buffer::new();
    if mount_vfs != 0
        && vela_user::file::fs_mount(mount_vfs, b"/sd", endpoint, &mut response).is_err()
    {
        sys::exit();
    }
    let mut pending = 0;
    loop {
        let ok = if pending == 0 {
            request.receive(endpoint)
        } else {
            request.reply_and_receive(pending, &response, endpoint)
        };
        if !ok {
            sys::exit();
        }
        pending = request.reply;
        if pending == 0 {
            continue;
        }
        response.clear();
        profiling.set(false);
        let result = (|| -> Result<(), (u8, &'static [u8])> {
            let req = request.as_slice();
            let op = *req.first().ok_or((
                fs_abi::STATUS_ERR,
                b"read-only or invalid request".as_slice(),
            ))?;
            let bulk_op = fs_abi::bulk_read_operation(op);
            let profiled = bulk_op.is_some() && op & fs_abi::BULK_READ_PROFILE != 0;
            if !matches!(
                op,
                fs_abi::OP_READ
                    | fs_abi::OP_LIST
                    | fs_abi::OP_WRITE
                    | fs_abi::OP_STAT
                    | fs_abi::OP_READ_BULK
                    | fs_abi::OP_READ_BULK_SINGLE
                    | fs_abi::OP_READ_BULK_STAGED
                    | fs_abi::OP_WRITE_BULK
                    | fs_abi::OP_UNLINK
                    | fs_abi::OP_MKDIR
            ) && bulk_op.is_none()
            {
                return Err((fs_abi::STATUS_ERR, b"unsupported request"));
            }
            profiling.set(profiled);

            let epoch = sys::call(abi::SD_PARTITION, storage, 0, 0, abi::SD_PARTITION_EPOCH);
            if (epoch as i32) <= 0 {
                return Err((fs_abi::STATUS_ERR, b"SD system partition unavailable"));
            }
            if epoch != mounted_epoch {
                mount = None;
                opened = None;
                directory = None;
                mounted_epoch = epoch;
            }
            if mount.is_none() {
                let sectors = sys::call(abi::SD_PARTITION, storage, 0, 0, 0);
                if (sectors as i32) <= 0 {
                    return Err((fs_abi::STATUS_ERR, b"SD system partition unavailable"));
                }
                mount = Some(
                    Ext4::mount(Partition {
                        profiled: &profiling,
                        handle: storage,
                        sectors,
                    })
                    .map_err(err_reply)?,
                );
            }
            let fs = mount.as_mut().unwrap();

            match req {
                [fs_abi::OP_STAT, path @ ..] if !path.is_empty() => {
                    let (size, is_dir) = fs.stat(path).map_err(err_reply)?;
                    response.push(fs_abi::STATUS_OK);
                    response.append(&size.to_le_bytes());
                    response.push(if is_dir {
                        fs_abi::KIND_DIR
                    } else {
                        fs_abi::KIND_FILE
                    });
                    Ok(())
                }
                [fs_abi::OP_LIST, a, b, c, d, path @ ..] if !path.is_empty() => {
                    let offset = u32::from_le_bytes([*a, *b, *c, *d]);
                    if offset == 0
                        || directory
                            .as_ref()
                            .is_none_or(|(key, _)| key.as_slice() != path)
                    {
                        let dir = fs.open_dir(path).map_err(err_reply)?;
                        let mut key = Buffer::new();
                        key.append(path);
                        directory = Some((key, dir));
                    }
                    let mut data = [0; abi::IPC_BYTES - fs_abi::DIR_HEADER];
                    let chunk = fs
                        .read_dir(&directory.as_ref().unwrap().1, offset, &mut data)
                        .map_err(err_reply)?;
                    response.push(fs_abi::STATUS_OK);
                    if let Some(chunk) = chunk {
                        response.append(&chunk.next.to_le_bytes());
                        response.push(u8::from(chunk.end_of_name));
                        response.append(&data[..chunk.len]);
                    }
                    Ok(())
                }
                [fs_abi::OP_READ, a, b, c, d, path @ ..] if !path.is_empty() => {
                    let offset = u32::from_le_bytes([*a, *b, *c, *d]);
                    if offset == 0
                        || opened
                            .as_ref()
                            .is_none_or(|(key, _)| key.as_slice() != path)
                    {
                        let file = fs.open(path).map_err(err_reply)?;
                        let mut key = Buffer::new();
                        key.append(path);
                        opened = Some((key, file));
                    }
                    let mut data = [0; abi::IPC_BYTES - 1];
                    let count = fs
                        .read(&opened.as_ref().unwrap().1, offset, &mut data)
                        .map_err(err_reply)?;
                    response.push(fs_abi::STATUS_OK);
                    response.append(&data[..count]);
                    Ok(())
                }
                [_op, a, b, c, d, e, f, g, h, path @ ..]
                    if bulk_op.is_some() && !path.is_empty() =>
                {
                    let mode = bulk_op.unwrap();
                    let profile_flag = if profiled { abi::IO_PROFILE } else { 0 };
                    let opened_at = if profiled { sys::monotonic_16m() } else { 0 };
                    let mut offset = u32::from_le_bytes([*a, *b, *c, *d]);
                    let max_len = u32::from_le_bytes([*e, *f, *g, *h]) as usize;
                    if offset == 0
                        || opened
                            .as_ref()
                            .is_none_or(|(key, _)| key.as_slice() != path)
                    {
                        let file = fs.open(path).map_err(err_reply)?;
                        let mut key = Buffer::new();
                        key.append(path);
                        opened = Some((key, file));
                    }
                    let file = &opened.as_ref().unwrap().1;
                    // SAFETY: single-threaded service, never re-entered.
                    let buf = unsafe { &mut (*core::ptr::addr_of_mut!(BULK_CHUNK)).0 };
                    let mut written = 0usize;
                    let mut read_ticks = if profiled {
                        sys::monotonic_16m() - opened_at
                    } else {
                        0
                    };
                    let mut copy_ticks = 0u64;
                    while written < max_len {
                        let mut want = (max_len - written).min(buf.len());
                        let read_at = if profiled { sys::monotonic_16m() } else { 0 };
                        if mode == fs_abi::OP_READ_BULK {
                            let Some(range) =
                                fs.read_chunk(file, offset, want).map_err(err_reply)?
                            else {
                                if profiled {
                                    read_ticks += sys::monotonic_16m() - read_at;
                                }
                                break;
                            };
                            want = range.len;
                            if let Some(address) = range.address {
                                if address.is_multiple_of(SECTOR as u64) {
                                    let sector = u32::try_from(address / SECTOR as u64)
                                        .map_err(|_| err_reply(Error::Corrupt))?;
                                    if sys::call6(
                                        abi::SD_PARTITION,
                                        storage,
                                        sector,
                                        pending,
                                        abi::SD_PARTITION_READ_BULK | profile_flag,
                                        want as u32,
                                        written as u32,
                                    )
                                    .0 != 0
                                    {
                                        return Err(err_reply(Error::Io));
                                    }
                                    if profiled {
                                        read_ticks += sys::monotonic_16m() - read_at;
                                    }
                                    written += want;
                                    offset += want as u32;
                                    continue;
                                }
                                // Stage only the unaligned leading bytes; the next
                                // iteration can use the delegated window directly.
                                want = want.min(SECTOR - (address % SECTOR as u64) as usize);
                            }
                            // Sparse ranges are zero-filled through the staged path.
                        }
                        let n = fs
                            .read_with_batch(
                                &file,
                                offset,
                                &mut buf[..want],
                                mode != fs_abi::OP_READ_BULK_SINGLE,
                            )
                            .map_err(err_reply)?;
                        if profiled {
                            read_ticks += sys::monotonic_16m() - read_at;
                        }
                        if n == 0 {
                            break;
                        }
                        let copied_at = if profiled { sys::monotonic_16m() } else { 0 };
                        if sys::ipc_bulk_write_with_profile(pending, written, &buf[..n], profiled)
                            .is_err()
                        {
                            return Err((fs_abi::STATUS_ERR, b"bulk write failed"));
                        }
                        if profiled {
                            copy_ticks += sys::monotonic_16m() - copied_at;
                        }
                        written += n;
                        offset = offset.saturating_add(n as u32);
                    }
                    response.push(fs_abi::STATUS_OK);
                    response.append(&(written as u32).to_le_bytes());
                    // Optional profiling suffix after the stable five-byte reply.
                    if profiled {
                        response.append(&read_ticks.to_le_bytes());
                        response.append(&copy_ticks.to_le_bytes());
                    }
                    Ok(())
                }
                [fs_abi::OP_WRITE, a, b, c, d, path_len, rest @ ..]
                    if usize::from(*path_len) > 0 && usize::from(*path_len) <= rest.len() =>
                {
                    let offset = u32::from_le_bytes([*a, *b, *c, *d]);
                    let plen = usize::from(*path_len);
                    let path = &rest[..plen];
                    let data = &rest[plen..];
                    opened = None;
                    directory = None;
                    fs.write(path, offset, data).map_err(err_reply)?;
                    response.push(fs_abi::STATUS_OK);
                    Ok(())
                }
                [fs_abi::OP_WRITE_BULK, a, b, c, d, e, f, g, h, path @ ..] if !path.is_empty() => {
                    let mut offset = u32::from_le_bytes([*a, *b, *c, *d]);
                    let total_len = u32::from_le_bytes([*e, *f, *g, *h]) as usize;
                    opened = None;
                    directory = None;
                    if total_len == 0 {
                        fs.write(path, offset, &[]).map_err(err_reply)?;
                        response.push(fs_abi::STATUS_OK);
                        response.append(&0u32.to_le_bytes());
                        return Ok(());
                    }
                    // SAFETY: single-threaded service, never re-entered.
                    let buf = unsafe { &mut (*core::ptr::addr_of_mut!(BULK_CHUNK)).0 };
                    let mut done = 0usize;
                    while done < total_len {
                        let step = (total_len - done).min(buf.len());
                        let Ok(n) = sys::ipc_bulk_read(pending, done, &mut buf[..step]) else {
                            return Err((fs_abi::STATUS_ERR, b"bulk read failed"));
                        };
                        if n == 0 {
                            break;
                        }
                        fs.write(path, offset, &buf[..n]).map_err(err_reply)?;
                        done += n;
                        offset = offset.saturating_add(n as u32);
                    }
                    response.push(fs_abi::STATUS_OK);
                    response.append(&(done as u32).to_le_bytes());
                    Ok(())
                }
                [fs_abi::OP_UNLINK, path @ ..] if !path.is_empty() => {
                    opened = None;
                    directory = None;
                    fs.unlink(path).map_err(err_reply)?;
                    response.push(fs_abi::STATUS_OK);
                    Ok(())
                }
                [fs_abi::OP_MKDIR, path @ ..] if !path.is_empty() => {
                    opened = None;
                    directory = None;
                    fs.mkdir(path).map_err(err_reply)?;
                    response.push(fs_abi::STATUS_OK);
                    Ok(())
                }
                _ => Err((fs_abi::STATUS_ERR, b"invalid request")),
            }
        })();
        if let Err((status, error)) = result {
            if error == message(Error::Io) {
                mount = None;
                opened = None;
                directory = None;
            }
            response.push(status);
            response.append(b"ext4: ");
            response.append(error);
            response.push(b'\n');
        }
    }
}

fn err_reply(error: Error) -> (u8, &'static [u8]) {
    let status = match error {
        Error::NotFound => fs_abi::STATUS_NOENT,
        _ => fs_abi::STATUS_ERR,
    };
    (status, message(error))
}

fn message(error: Error) -> &'static [u8] {
    match error {
        Error::Io => b"read error",
        Error::Corrupt => b"invalid filesystem metadata",
        Error::Unsupported => b"unsupported filesystem features or unclean volume",
        Error::NotFound => b"file not found",
        Error::NotFile => b"not a regular file",
        Error::NotDirectory => b"not a directory",
        Error::NoSpace => b"no space left on device",
        Error::AlreadyExists => b"file or directory already exists",
        Error::NotEmpty => b"directory not empty",
    }
}
