#![no_std]
#![no_main]

use vela_sdk::{App, Result, io, memory::Memory};

vela_sdk::entry!(main);

fn output(bytes: &mut [u8]) -> Result<()> {
    bytes.make_ascii_uppercase();
    io::write(bytes)
}

fn main(app: App) -> Result<()> {
    let argument = app.argument();
    if argument.is_empty() && !io::has_stdin() {
        return io::write(b"upper: pass text, an absolute file path, or piped input\n");
    }
    let mut memory = Memory::new(1024)?;
    let buffer = memory.as_mut_slice();
    if argument.starts_with(b"/") {
        let mut file = app.files().open(argument)?;
        loop {
            let count = file.read(buffer)?;
            if count == 0 {
                break;
            }
            output(&mut buffer[..count])?;
        }
    } else if !argument.is_empty() {
        buffer[..argument.len()].copy_from_slice(argument);
        output(&mut buffer[..argument.len()])?;
        io::write(b"\n")?;
    } else {
        loop {
            let count = io::read(buffer)?;
            if count == 0 {
                break;
            }
            output(&mut buffer[..count])?;
        }
    }
    Ok(())
}
