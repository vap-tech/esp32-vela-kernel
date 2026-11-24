#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{Buffer, div64, dword_le, sys};

vela_user::entry!(main);

fn main(_fs: u32, _arg: u32, _len: u32, _a3: u32, _a4: u32, _a5: u32) -> ! {
    let mut stats = [0; abi::STATS_BYTES];
    if sys::call(
        abi::STATS,
        stats.as_mut_ptr() as u32,
        stats.len() as u32,
        abi::STATS_SUMMARY,
        0,
    ) == 0
    {
        let (lo, hi) = dword_le(&stats, 24);
        let (seconds_lo, seconds_hi, hundredths) = div64(lo, hi, 100);
        let mut out = Buffer::new();
        out.decimal64(seconds_lo, seconds_hi);
        out.push(b'.');
        out.push(b'0' + (hundredths / 10) as u8);
        out.push(b'0' + (hundredths % 10) as u8);
        out.push(b'\n');
        sys::write(out.as_slice());
    } else {
        sys::write(b"uptime: kernel statistics unavailable\n");
    }
    sys::process_exit();
}
