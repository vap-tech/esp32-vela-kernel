#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{
    Buffer, TextBuf,
    file::{DIR_HEADER, fs_read_dir},
    sys,
};

vela_user::entry!(main);

fn main(fs: u32, arg: u32, len: u32, _a3: u32, _a4: u32, _a5: u32) -> ! {
    if fs == 0 || len as usize > abi::SPAWN_ARG_MAX || (len != 0 && arg == 0) {
        sys::write(b"usage: ls [</directory>]\n");
        sys::process_exit();
    }
    let path = if len == 0 {
        b"/" as &[u8]
    } else {
        // SAFETY: SPAWN_IMAGE placed this read-only argument above our initial SP.
        unsafe { core::slice::from_raw_parts(arg as *const u8, len as usize) }
    };
    if path.starts_with(b"/") {
        list(fs, path);
    } else {
        sys::write(b"usage: ls [</directory>]\n");
    }
    sys::process_exit();
}

fn list(fs: u32, path: &[u8]) {
    let mut cursor = 0;
    let mut response = Buffer::new();
    let mut line = TextBuf::<{ (abi::IPC_BYTES - DIR_HEADER) * 4 + 1 }>::new();
    while let Ok(Some((next, end_of_name))) = fs_read_dir(fs, path, cursor, &mut response) {
        line.clear();
        for &byte in &response.as_slice()[DIR_HEADER..] {
            match byte {
                b'\\' => line.append(b"\\\\"),
                b' '..=b'~' => line.push(byte),
                _ => {
                    // Keep control bytes and invalid UTF-8 out of the terminal.
                    let hex = b"0123456789abcdef";
                    line.append(b"\\x");
                    line.push(hex[(byte >> 4) as usize]);
                    line.push(hex[(byte & 15) as usize]);
                }
            }
        }
        if end_of_name {
            line.push(b'\n');
        }
        sys::write(line.as_slice());
        cursor = next;
    }
}
