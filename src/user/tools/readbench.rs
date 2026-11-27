#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{Buffer, TextBuf, arena::Arena, file::fs_read_bulk, io, sys, word_le};

vela_user::entry!(main);

fn snapshot() -> [u8; abi::STATS_SDCARD_IO_BYTES] {
    let mut data = [0; abi::STATS_SDCARD_IO_BYTES];
    assert_eq!(
        sys::call(
            abi::STATS,
            data.as_mut_ptr() as u32,
            data.len() as u32,
            abi::STATS_SDCARD,
            0
        ),
        0
    );
    data
}

fn main(fs: u32, arg: u32, len: u32, stdin: u32, stdout: u32, _: u32) -> ! {
    io::init(stdin, stdout);
    if fs == 0 || arg == 0 || len == 0 || len as usize > abi::SPAWN_ARG_MAX {
        io::ewrite(b"usage: readbench /sd/<file>\n");
        sys::process_exit();
    }
    // SAFETY: the loader reserves the argument above the initial stack.
    let path = unsafe { core::slice::from_raw_parts(arg as *const u8, len as usize) };
    let Ok(mut arena) = Arena::new_psram(4096) else {
        io::ewrite(b"readbench: allocation failed\n");
        sys::process_exit();
    };
    // SAFETY: privately owned live allocation, released after the last access.
    let chunk = unsafe { core::slice::from_raw_parts_mut(arena.base() as *mut u8, 4096) };
    let mut response = Buffer::new();
    let before = snapshot();
    let start = sys::call(abi::TICKS, 0, 0, 0, 0);
    let mut offset = 0u32;
    let mut hash = 2166136261u32;
    loop {
        match fs_read_bulk(fs, path, offset, chunk, &mut response) {
            Ok(0) => break,
            Ok(n) => {
                for &b in &chunk[..n] {
                    hash = (hash ^ u32::from(b)).wrapping_mul(16777619);
                }
                offset += n as u32;
            }
            Err(_) => {
                io::ewrite(response.as_slice());
                sys::process_exit();
            }
        }
    }
    let ticks = sys::call(abi::TICKS, 0, 0, 0, 0).wrapping_sub(start);
    let after = snapshot();
    let mut out = TextBuf::<192>::new();
    for (label, value) in [
        (b"bytes=" as &[u8], offset),
        (b" ticks=", ticks),
        (
            b" sectors=",
            word_le(&after, 20).wrapping_sub(word_le(&before, 20)),
        ),
        (
            b" commands=",
            word_le(&after, 44).wrapping_sub(word_le(&before, 44)),
        ),
        (b" fnv1a=", hash),
    ] {
        out.append(label);
        out.decimal(value);
    }
    out.push(b'\n');
    let _ = io::write(out.as_slice());
    let _ = arena.release();
    sys::process_exit();
}
