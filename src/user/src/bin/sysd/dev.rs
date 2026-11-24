//! `/dev/*` base device nodes (`i2c-0`, `i2c-1`, `kbdled`, `random`, `urandom`, `zero`).

use vela_abi::{fs as fs_abi, syscall as abi};
use vela_user::{TextBuf, dword_le, i2c, kbd, sys, word_le};

pub const ENTRIES: &[&[u8]] = &[
    b"i2c-0", b"i2c-1", b"kbdled", b"random", b"urandom", b"zero",
];

#[derive(Clone, Copy)]
pub struct DevCaps {
    pub i2c_system: u32,
    pub i2c_ext1: u32,
}

impl DevCaps {
    fn bus(self, id: u8) -> Option<u32> {
        match id {
            abi::I2C_BUS_SYSTEM => Some(self.i2c_system),
            abi::I2C_BUS_EXT1 => Some(self.i2c_ext1),
            _ => None,
        }
    }
}

pub fn read(rel: &[u8], caps: DevCaps, out: &mut TextBuf<1024>) -> u8 {
    match rel {
        b"random" => render_random(b"/dev/random", out),
        b"urandom" => render_random(b"/dev/urandom", out),
        b"zero" => {
            out.append(b"device /dev/zero\nsource kernel_zero_stream\nzero_256b");
            for _ in 0..8 {
                out.append(b" 0x00000000");
            }
            out.push(b'\n');
            fs_abi::STATUS_OK
        }
        b"kbdled" => kbd_led_op(None, caps.i2c_ext1, out),
        b"i2c-0" | b"i2c-1" => {
            let id = rel[4] - b'0';
            match (i2c::bus_info(id), caps.bus(id)) {
                (Some(info), Some(cap)) => {
                    i2c::render_scan_sys(out, info, cap);
                    fs_abi::STATUS_OK
                }
                _ => {
                    out.append(b"dev: no such device\n");
                    fs_abi::STATUS_NOENT
                }
            }
        }
        _ => {
            out.append(b"dev: no such device\n");
            fs_abi::STATUS_NOENT
        }
    }
}

pub fn write(rel: &[u8], data: &[u8], caps: DevCaps, out: &mut TextBuf<1024>) -> u8 {
    match rel {
        b"kbdled" => kbd_led_op(Some(data), caps.i2c_ext1, out),
        b"i2c-0" | b"i2c-1" | b"random" | b"urandom" | b"zero" => {
            out.append(b"dev: read-only device\n");
            fs_abi::STATUS_ERR
        }
        _ => {
            out.append(b"dev: no such device\n");
            fs_abi::STATUS_NOENT
        }
    }
}

#[inline(never)]
fn render_random(path: &[u8], out: &mut TextBuf<1024>) -> u8 {
    let mut snap = [0u8; abi::STATS_RANDOM_BYTES];
    if sys::call(
        abi::STATS,
        snap.as_mut_ptr() as u32,
        abi::STATS_RANDOM_BYTES as u32,
        abi::STATS_RANDOM,
        0,
    ) != 0
    {
        out.append(b"service unavailable\n");
        return fs_abi::STATUS_ERR;
    }
    let cfg = word_le(&snap, 0);
    let last_raw = word_le(&snap, 4);
    let (s_lo, s_hi) = dword_le(&snap, 8);
    out.append(b"device ");
    out.append(path);
    out.append(b"\nsource esp32p4_lp_trng_0x501101a4\nrng_cfg ");
    out.hex16(cfg as u16);
    out.append(b"\nlast_raw ");
    out.hex32(last_raw);
    out.append(b"\nhw_samples ");
    out.decimal64(s_lo, s_hi);
    out.append(b"\nentropy_256b ");
    for i in 0..8 {
        if i > 0 {
            out.push(b' ');
        }
        out.hex32(word_le(&snap, 16 + i * 4));
    }
    out.push(b'\n');
    fs_abi::STATUS_OK
}

#[inline(never)]
fn kbd_led_op(args: Option<&[u8]>, ext1: u32, out: &mut TextBuf<1024>) -> u8 {
    let mut bus = kbd::SysBus(ext1);
    if let Some(args) = args {
        let trimmed = args.trim_ascii();
        if !trimmed.is_empty() {
            let Some(command) = kbd::parse_led_command(trimmed) else {
                out.append(b"kbd led: bad args (auto|off|bright 0..100|R G B [R G B])\n");
                return fs_abi::STATUS_ERR;
            };
            if kbd::apply_led(&mut bus, command).is_err() {
                out.append(b"kbd led: no keyboard\n");
                return fs_abi::STATUS_ERR;
            }
        }
    }
    let Ok(status) = kbd::read_led_status(&mut bus) else {
        out.append(b"kbd led: no keyboard\n");
        return fs_abi::STATUS_ERR;
    };
    out.append(if status.custom {
        b"kbd led: mode=custom bright="
    } else {
        b"kbd led: mode=auto bright="
    });
    out.decimal(u32::from(status.brightness));
    for (index, color) in status.colors.iter().enumerate() {
        out.append(if index == 0 { b" rgb1=" } else { b" rgb2=" });
        for channel in [color.r, color.g, color.b] {
            for nibble in [channel >> 4, channel & 15] {
                out.push(b"0123456789abcdef"[usize::from(nibble)]);
            }
        }
    }
    out.push(b'\n');
    fs_abi::STATUS_OK
}
