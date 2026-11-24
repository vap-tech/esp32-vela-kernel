#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::sys;

vela_user::entry!(main);

fn parse_u32_range(bytes: &[u8], min: u32, max: u32) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut val = 0u32;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    (val >= min && val <= max).then_some(val)
}

fn main(_vfs: u32, arg: u32, len: u32, display_cap: u32, _a4: u32, _a5: u32) -> ! {
    if display_cap == 0 || len as usize > abi::SPAWN_ARG_MAX || (len != 0 && arg == 0) {
        sys::write(b"sdexport: display capability unavailable\n");
        sys::process_exit();
    }
    let text = if len == 0 {
        b"" as &[u8]
    } else {
        // SAFETY: SPAWN_IMAGE placed this read-only argument above our initial SP.
        unsafe { core::slice::from_raw_parts(arg as *const u8, len as usize) }.trim_ascii()
    };
    let writable = text == b"rw";
    let seconds = if writable || text.is_empty() {
        60
    } else if let Some(seconds) = parse_u32_range(text, 1, 600) {
        seconds
    } else {
        sys::write(b"usage: sdexport [seconds 1..600 | rw]\n");
        sys::process_exit();
    };
    let operation = if writable {
        sys::write(b"USB-C writable SD export. Unmount and eject on host.\n");
        abi::SD_EXPORT_START_RW
    } else {
        sys::write(b"USB-C read-only SD export.\n");
        abi::SD_EXPORT_START
    };
    if sys::call(abi::SD_EXPORT, display_cap, operation, 0, 0) != 0 {
        sys::write(b"sdexport: could not start USB OTG\n");
        sys::process_exit();
    }
    let start = sys::call(abi::TICKS, 0, 0, 0, 0);
    while writable || sys::call(abi::TICKS, 0, 0, 0, 0).wrapping_sub(start) < seconds * 100 {
        match sys::call(abi::SD_EXPORT, display_cap, abi::SD_EXPORT_POLL, 0, 0) {
            0 => sys::sleep_ticks(1),
            1 => {
                sys::call(abi::YIELD, 0, 0, 0, 0);
            }
            abi::SD_EXPORT_EJECTED => {
                sys::write(b"SD ejected; back under Vela control\n");
                sys::process_exit();
            }
            _ => {
                sys::write(b"sdexport: USB transport error\n");
                sys::process_exit();
            }
        }
    }
    let _ = sys::call(abi::SD_EXPORT, display_cap, abi::SD_EXPORT_STOP, 0, 0);
    sys::write(b"sdexport ended\n");
    sys::process_exit();
}
