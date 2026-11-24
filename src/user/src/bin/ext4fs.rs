#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{
    Buffer,
    ext4::{BlockRead, Error, Ext4, File, SECTOR},
    sys,
};

vela_user::entry!(main);

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
        let result = (|| {
            let [0, a, b, c, d, path @ ..] = request.as_slice() else {
                return Err(b"invalid request" as &[u8]);
            };
            let offset = u32::from_le_bytes([*a, *b, *c, *d]);
            let epoch = sys::call(abi::SD_PARTITION, storage, 0, 0, abi::SD_PARTITION_EPOCH);
            if (epoch as i32) <= 0 {
                return Err(b"SD system partition unavailable");
            }
            if epoch != mounted_epoch {
                mount = None;
                opened = None;
                mounted_epoch = epoch;
            }
            if mount.is_none() {
                let sectors = sys::call(abi::SD_PARTITION, storage, 0, 0, 0);
                if (sectors as i32) <= 0 {
                    return Err(b"SD system partition unavailable");
                }
                mount = Some(
                    Ext4::mount(Partition {
                        handle: storage,
                        sectors,
                    })
                    .map_err(message)?,
                );
            }
            let fs = mount.as_mut().unwrap();
            if offset == 0
                || opened
                    .as_ref()
                    .is_none_or(|(key, _)| key.as_slice() != path)
            {
                let file = fs.open(path).map_err(message)?;
                let mut key = Buffer::new();
                key.append(path);
                opened = Some((key, file));
            }
            let mut data = [0; abi::IPC_BYTES - 1];
            let count = fs
                .read(&opened.as_ref().unwrap().1, offset, &mut data)
                .map_err(message)?;
            response.push(0);
            response.append(&data[..count]);
            Ok(())
        })();
        if let Err(error) = result {
            if error == message(Error::Io) {
                mount = None;
                opened = None;
            }
            response.push(1);
            response.append(b"ext4: ");
            response.append(error);
            response.push(b'\n');
        }
    }
}

fn message(error: Error) -> &'static [u8] {
    match error {
        Error::Io => b"read error",
        Error::Corrupt => b"invalid filesystem metadata",
        Error::Unsupported => b"unsupported filesystem features or unclean volume",
        Error::NotFound => b"file not found",
        Error::NotFile => b"not a regular file",
    }
}
