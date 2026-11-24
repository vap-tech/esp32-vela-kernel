#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{file::cat, sys};

vela_user::entry!(main);

fn main(fs: u32, arg: u32, len: u32, _a3: u32, _a4: u32, _a5: u32) -> ! {
    if fs == 0 || arg == 0 || len == 0 || len as usize > abi::SPAWN_ARG_MAX {
        sys::write(b"usage: cat <path>\n");
    } else {
        // SAFETY: SPAWN_IMAGE reserves these bytes above our initial SP.
        // This program is single-threaded and never mutates the argument.
        let path = unsafe { core::slice::from_raw_parts(arg as *const u8, len as usize) };
        if path.starts_with(b"/") {
            cat(fs, path);
        } else {
            sys::write(b"usage: cat </path>\n");
        }
    }
    sys::process_exit();
}
