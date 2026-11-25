#![no_std]
#![no_main]

use vela_abi::{fs as fs_abi, syscall as abi};
use vela_user::{
    Buffer,
    ext4::{BlockRead, Directory, Error, Ext4, File, SECTOR},
    sys,
};

vela_user::entry!(main);

/// NOLOAD `.bss` staging buffer for `OP_READ_BULK` (keeps the 4 KiB service stack clear).
static mut BULK_CHUNK: [u8; 4096] = [0; 4096];

struct Partition {
    handle: u32,
    sectors: u32,
}

impl BlockRead for Partition {
    fn sectors(&self) -> u32 {
        self.sectors
    }

    fn read_sector(&mut self, sector: u32, out: &mut [u8; SECTOR]) -> Result<(), Error> {
        if sys::call(
            abi::SD_PARTITION,
            self.handle,
            sector,
            out.as_mut_ptr() as u32,
            0,
        ) == 0
        {
            Ok(())
        } else {
            Err(Error::Io)
        }
    }
}

fn main(endpoint: u32, storage: u32, _a2: u32, _a3: u32, _a4: u32, _a5: u32) -> ! {
    let mut mount = None;
    let mut mounted_epoch = 0;
    let mut opened: Option<(Buffer, File)> = None;
    let mut directory: Option<(Buffer, Directory)> = None;
    let mut request = Buffer::new();
    let mut response = Buffer::new();
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
        let result = (|| -> Result<(), (u8, &'static [u8])> {
            let req = request.as_slice();
            let op = *req.first().ok_or((
                fs_abi::STATUS_ERR,
                b"read-only or invalid request".as_slice(),
            ))?;
            if !matches!(
                op,
                fs_abi::OP_READ | fs_abi::OP_LIST | fs_abi::OP_STAT | fs_abi::OP_READ_BULK
            ) {
                return Err((fs_abi::STATUS_ERR, b"read-only or invalid request"));
            }

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
                [fs_abi::OP_READ_BULK, a, b, c, d, e, f, g, h, path @ ..] if !path.is_empty() => {
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
                    let buf = unsafe { &mut *core::ptr::addr_of_mut!(BULK_CHUNK) };
                    let mut written = 0usize;
                    while written < max_len {
                        let want = (max_len - written).min(buf.len());
                        let n = fs
                            .read(&file, offset, &mut buf[..want])
                            .map_err(err_reply)?;
                        if n == 0 {
                            break;
                        }
                        if sys::ipc_bulk_write(pending, written, &buf[..n]).is_err() {
                            return Err((fs_abi::STATUS_ERR, b"bulk write failed"));
                        }
                        written += n;
                        offset = offset.saturating_add(n as u32);
                    }
                    response.push(fs_abi::STATUS_OK);
                    response.append(&(written as u32).to_le_bytes());
                    Ok(())
                }
                _ => Err((fs_abi::STATUS_ERR, b"read-only or invalid request")),
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
    }
}
