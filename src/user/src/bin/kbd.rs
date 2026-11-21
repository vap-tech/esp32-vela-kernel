#![no_std]
#![no_main]

//! Tab5 Keyboard input service: polls the Character-mode FIFO on Ext.Port1 and forwards
//! the characters to the shell's input endpoint, next to the USB console.

use vela_user::{
    Buffer, TextBuf,
    kbd::{self, Poll, SysBus},
    sys,
};

vela_user::entry!(main);

/// Poll period in 10 ms ticks. An idle poll is one register read (~1 ms of bit-banged I2C),
/// so 30 ms keeps the CPU cost at a few percent while the FIFO (32 events) never overflows.
const POLL_TICKS: u32 = 3;
/// How often to look for a keyboard that is absent or was unplugged.
const RECHECK_TICKS: u32 = 200;
/// Events forwarded per poll, so a flood cannot starve other tasks.
const MAX_EVENTS_PER_POLL: usize = 8;

fn log_unmapped(modifier: u8, chars: &[u8]) {
    let mut line = TextBuf::<96>::new();
    line.append(b"[kbd] unmapped mod=");
    line.hex8(modifier);
    line.append(b" bytes");
    for &c in chars {
        line.push(b' ');
        line.hex8(c);
    }
    line.push(b'\n');
    sys::write(line.as_slice());
}

fn log_bad_length(len: u8, flushed: bool) {
    let mut line = TextBuf::<64>::new();
    line.append(b"[kbd] bad event length ");
    line.hex8(len);
    line.append(if flushed {
        b", queue flushed\n"
    } else {
        b", retry\n"
    });
    sys::write(line.as_slice());
}

/// Consecutive bad length reads after which the queue is flushed.
const BAD_LENGTH_FLUSH: u32 = 3;

fn serve(bus: &mut SysBus, input_handle: u32) {
    let mut bad_length = 0u32;
    loop {
        let mut out = Buffer::new();
        for _ in 0..MAX_EVENTS_PER_POLL {
            match kbd::poll(bus) {
                Ok(Poll::Empty) => {
                    bad_length = 0;
                    break;
                }
                Ok(Poll::Event(event)) => {
                    bad_length = 0;
                    let mut bytes = [0u8; kbd::MAX_OUT];
                    match kbd::translate(&event, &mut bytes) {
                        Some(n) => out.append(&bytes[..n]),
                        None => log_unmapped(event.modifier, event.raw()),
                    }
                }
                Ok(Poll::BadLength(len)) => {
                    // The cause is not known yet: retry first, drop the queue only if it sticks.
                    bad_length += 1;
                    let flush = bad_length >= BAD_LENGTH_FLUSH;
                    log_bad_length(len, flush);
                    if flush {
                        bad_length = 0;
                        if kbd::flush(bus).is_err() {
                            return;
                        }
                    }
                    break;
                }
                Err(_) => {
                    sys::write(b"[kbd] lost\n");
                    return;
                }
            }
        }
        // A full shell queue only drops keystrokes; the driver keeps running.
        if !out.is_empty() {
            let _ = out.send(input_handle);
        }
        sys::sleep_ticks(POLL_TICKS);
    }
}

fn main(input_handle: u32, i2c_ext1: u32, _a2: u32, _a3: u32, _a4: u32, _a5: u32) -> ! {
    let mut bus = SysBus(i2c_ext1);
    loop {
        if let Ok(f0) = kbd::init(&mut bus) {
            let mut line = TextBuf::<64>::new();
            line.append(b"[kbd] Tab5 Keyboard 0x6d reg_f0=");
            line.hex8(f0);
            line.append(b" character mode\n");
            sys::write(line.as_slice());
            serve(&mut bus, input_handle);
        }
        sys::sleep_ticks(RECHECK_TICKS);
    }
}
