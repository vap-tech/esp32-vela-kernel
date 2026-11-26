#![no_std]
#![no_main]

//! A small filesystem API exercise. Refuses to overwrite an existing test file.
use vela_sdk::{App, Error, Result, io, memory::Memory};

vela_sdk::entry!(main);

fn main(app: App) -> Result<()> {
    let files = app.files();
    let path = b"/sd/vela-sdk-example.txt";
    match files.metadata(path) {
        Err(Error::NotFound) => {}
        Ok(_) => {
            io::ewrite(b"files: test path already exists, leaving it untouched\n");
            return Err(Error::InvalidArgument);
        }
        Err(error) => return Err(error),
    }
    let mut file = files.create(path)?;
    let result = (|| {
        file.write(b"vela sdk\n")?;
        file.write(b"second line\n")?;
        if file.position() != 21 {
            return Err(Error::Io);
        }
        let mut memory = Memory::new(1024)?;
        let buffer = memory.as_mut_slice();
        file.seek(0);
        let count = file.read(buffer)?;
        if &buffer[..count] != b"vela sdk\nsecond line\n" {
            return Err(Error::Io);
        }
        if file.read(buffer)? != 0 {
            return Err(Error::Io);
        }
        file.seek(9);
        let count = file.read(buffer)?;
        if &buffer[..count] != b"second line\n" {
            return Err(Error::Io);
        }
        // Creating an existing file truncates it, matching Vela File IPC.
        let mut replacement = files.create(path)?;
        replacement.write(b"ok\n")?;
        if files.metadata(path)?.size != 3 {
            return Err(Error::Io);
        }
        Ok(())
    })();
    let cleanup = files.remove(path);
    result?;
    cleanup?;
    io::write(b"SDK files and memory OK\n")
}
