#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{file::cat, io, sys};

vela_user::entry!(main);

fn main(fs: u32, arg: u32, len: u32, stdin: u32, stdout: u32, _a5: u32) -> ! {
    io::init(stdin, stdout);
    if len == 0 && stdin != 0 {
        let mut buf = [0u8; abi::IPC_BYTES];
        while let Ok(n) = io::read(&mut buf) {
            if n == 0 || io::write(&buf[..n]).is_err() {
                break;
            }
        }
    } else if fs == 0 || arg == 0 || len == 0 || len as usize > abi::SPAWN_ARG_MAX {
        io::ewrite(b"usage: cat <path>\n");
    } else {
        // SAFETY: SPAWN_IMAGE reserves these bytes above our initial SP.
        // This program is single-threaded and never mutates the argument.
        let path = unsafe { core::slice::from_raw_parts(arg as *const u8, len as usize) };
        if path.starts_with(b"/") {
            cat(fs, path);
        } else {
            io::ewrite(b"usage: cat </path>\n");
        }
    }
    io::close_stdout();
    sys::process_exit();
}
