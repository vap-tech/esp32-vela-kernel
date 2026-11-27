#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{
    Buffer, TextBuf,
    arena::Arena,
    file::{fs_read_bulk_mode, fs_read_bulk_staged},
    io, sys, word_le,
};

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

fn window(cursor: u32, start: bool) -> [u8; abi::STATS_IRQ_WINDOW_BYTES] {
    let mut data = [0; abi::STATS_IRQ_WINDOW_BYTES];
    if sys::call6(
        abi::STATS,
        data.as_mut_ptr() as u32,
        data.len() as u32,
        abi::STATS_IRQ_WINDOW,
        cursor,
        u32::from(start),
        0,
    )
    .0 != 0
    {
        io::ewrite(b"readbench: IRQ history lost; measurement invalid\n");
        sys::process_exit();
    }
    data
}

fn counter(data: &[u8]) -> u64 {
    u64::from(word_le(data, 12)) | (u64::from(word_le(data, 16)) << 32)
}

fn profile() -> [u8; abi::STATS_READ_PROFILE_BYTES] {
    let mut data = [0; abi::STATS_READ_PROFILE_BYTES];
    assert_eq!(
        sys::call(
            abi::STATS,
            data.as_mut_ptr() as u32,
            data.len() as u32,
            abi::STATS_READ_PROFILE,
            0
        ),
        0
    );
    data
}

fn wide(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().unwrap())
}

fn report(label: &[u8], value: u64) {
    let mut out = TextBuf::<96>::new();
    out.append(label);
    out.decimal64(value as u32, (value >> 32) as u32);
    out.push(b'\n');
    let _ = io::write(out.as_slice());
}

fn main(fs: u32, arg: u32, len: u32, stdin: u32, stdout: u32, _: u32) -> ! {
    io::init(stdin, stdout);
    if fs == 0 || arg == 0 || len == 0 || len as usize > abi::SPAWN_ARG_MAX {
        io::ewrite(b"usage: readbench [verify] [single|staged] /sd/<file>\n");
        sys::process_exit();
    }
    // SAFETY: the loader reserves the argument above the initial stack.
    let argument = unsafe { core::slice::from_raw_parts(arg as *const u8, len as usize) };
    let (hash_enabled, argument) = match argument.strip_prefix(b"verify ") {
        Some(path) => (true, path),
        None => (false, argument.strip_prefix(b"io ").unwrap_or(argument)),
    };
    let (batch, staged, path) = if let Some(path) = argument.strip_prefix(b"single ") {
        (false, false, path)
    } else if let Some(path) = argument.strip_prefix(b"staged ") {
        (true, true, path)
    } else {
        (true, false, argument)
    };
    if !path.starts_with(b"/sd/") {
        io::ewrite(b"usage: readbench [verify] [single|staged] /sd/<file>\n");
        sys::process_exit();
    }
    let Ok(mut arena) = Arena::new_psram(4096) else {
        io::ewrite(b"readbench: allocation failed\n");
        sys::process_exit();
    };
    // SAFETY: privately owned live allocation, released after the last access.
    let chunk = unsafe { core::slice::from_raw_parts_mut(arena.base() as *mut u8, 4096) };
    let mut response = Buffer::new();
    let before = snapshot();
    let kernel_before = profile();
    let mut rpc_ticks = 0u64;
    let mut hash_ticks = 0u64;
    let mut ext4_ticks = 0u64;
    let mut service_copy_ticks = 0u64;
    let mut forward_ticks = 0u64;
    let mut profiled_replies = 0u32;
    let first = window(0, true);
    let start = counter(&first);
    let mut cursor = word_le(&first, 0);
    let mut samples = 0u32;
    let mut max_delay = 0u32;
    let mut end;
    let mut offset = 0u32;
    let mut hash = 2166136261u32;
    loop {
        if hash_enabled {
            chunk.fill(0xa5);
        }
        // Verification also exercises a grant ending inside a sector, then
        // unaligned file offsets. The timed default retains 4 KiB requests.
        let want = if hash_enabled && offset == 0 {
            chunk.len() - 1
        } else {
            chunk.len()
        };
        let rpc_at = sys::monotonic_16m();
        let result = if staged {
            fs_read_bulk_staged(fs, path, offset, &mut chunk[..want], &mut response)
        } else {
            fs_read_bulk_mode(fs, path, offset, &mut chunk[..want], &mut response, batch)
        };
        rpc_ticks += sys::monotonic_16m() - rpc_at;
        if response.get(0) == 0 && response.len() == vela_abi::fs::BULK_READ_PROFILE_BYTES {
            ext4_ticks += wide(response.as_slice(), 5);
            service_copy_ticks += wide(response.as_slice(), 13);
            forward_ticks += wide(response.as_slice(), 21);
            profiled_replies += 1;
        }
        let observed = window(cursor, false);
        cursor = word_le(&observed, 0);
        samples += word_le(&observed, 4);
        max_delay = max_delay.max(word_le(&observed, 8));
        end = counter(&observed);
        if let Ok(n) = result
            && hash_enabled
            && chunk[n..].iter().any(|&b| b != 0xa5)
        {
            io::ewrite(b"readbench: data written past returned length\n");
            sys::process_exit();
        }
        match result {
            Ok(0) => break,
            Ok(n) => {
                if hash_enabled {
                    let hash_at = sys::monotonic_16m();
                    for &b in &chunk[..n] {
                        hash = (hash ^ u32::from(b)).wrapping_mul(16777619);
                    }
                    hash_ticks += sys::monotonic_16m() - hash_at;
                }
                offset += n as u32;
            }
            Err(_) => {
                io::ewrite(response.as_slice());
                sys::process_exit();
            }
        }
    }
    let elapsed_us = ((end - start) / 16) as u32;
    let after = snapshot();
    let kernel_after = profile();
    let mut out = TextBuf::<256>::new();
    out.append(if staged {
        b"staged "
    } else if batch {
        b"direct "
    } else {
        b"single "
    });
    for (label, value) in [
        (b"bytes=" as &[u8], offset),
        (b" us=", elapsed_us),
        (b" irq_samples=", samples),
        (b" irq_max_us=", max_delay.div_ceil(16)),
        (
            b" sectors=",
            word_le(&after, 20).wrapping_sub(word_le(&before, 20)),
        ),
        (
            b" commands=",
            word_le(&after, 44).wrapping_sub(word_le(&before, 44)),
        ),
    ] {
        out.append(label);
        out.decimal(value);
    }
    if hash_enabled {
        out.append(b" fnv1a=");
        out.decimal(hash);
    } else {
        out.append(b" hash=off");
    }
    out.push(b'\n');
    let _ = io::write(out.as_slice());
    for (label, ticks) in [
        (b"profile rpc_us=" as &[u8], rpc_ticks),
        (b"profile hash_us=", hash_ticks),
        (b"profile ext4_read_us=", ext4_ticks),
        (b"profile service_bulk_us=", service_copy_ticks),
        (b"profile sysd_forward_us=", forward_ticks),
        (
            b"profile sd_driver_us=",
            wide(&kernel_after, 0) - wide(&kernel_before, 0),
        ),
        (
            b"profile sd_copy_us=",
            wide(&kernel_after, 8) - wide(&kernel_before, 8),
        ),
        (
            b"profile kernel_bulk_copy_us=",
            wide(&kernel_after, 16) - wide(&kernel_before, 16),
        ),
    ] {
        report(label, ticks / 16);
    }
    report(b"profile replies=", u64::from(profiled_replies));
    report(
        b"profile sd_calls=",
        wide(&kernel_after, 24) - wide(&kernel_before, 24),
    );
    report(
        b"profile bulk_calls=",
        wide(&kernel_after, 32) - wide(&kernel_before, 32),
    );
    let _ = arena.release();
    sys::process_exit();
}
