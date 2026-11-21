#![no_std]
#![no_main]

//! Tab5 Keyboard input service: drains the Character-mode FIFO on Ext.Port1 and forwards
//! the characters to the shell's input endpoint, next to the USB console.
//!
//! The keyboard pulls `INT` (GPIO50) low while its queue is not empty. The kernel turns that
//! level into an event, so the service sleeps in `EVENT_WAIT` and costs no CPU while idle.

use vela_abi::syscall as abi;
use vela_user::{
    Buffer, TextBuf,
    fbcon::IDLE_TICKS,
    kbd::{self, LedCommand, Poll, SysBus},
    sys,
};

vela_user::entry!(main);

/// Longest single `event_wait` (the kernel accepts 1..=6000 ticks); the idle deadline is
/// reached over several waits.
const MAX_WAIT_TICKS: u32 = 6000;
/// A key that wakes a blanked screen is not typed; the service writes this no-op escape to
/// the console instead, which is the activity that makes `vela-fbcon` light the panel.
const WAKE_SEQUENCE: &[u8] = b"\x1b[m";
/// Pause after a wake-up that found no event, so a stuck `INT` cannot spin the CPU.
const WAKE_RETRY_TICKS: u32 = 1;
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

fn now() -> u32 {
    sys::call(abi::TICKS, 0, 0, 0, 0)
}

fn serve(bus: &mut SysBus, input_handle: u32, int_event: u32) {
    // Brightness to restore while the LEDs are dimmed after a long time without a key.
    let mut dimmed: Option<u8> = None;
    let mut last_key = now();
    let mut bad_length = 0u32;
    // The firmware holds INT low until INT_STA is cleared, even once the queue is empty;
    // while events remain it raises the line again within a few milliseconds.
    let mut released = false;
    let mut got_events = false;
    loop {
        let mut out = Buffer::new();
        let mut drained = false;
        let mut events = 0u32;
        for _ in 0..MAX_EVENTS_PER_POLL {
            match kbd::poll(bus) {
                Ok(Poll::Empty) => {
                    bad_length = 0;
                    drained = true;
                    break;
                }
                Ok(Poll::Event(event)) => {
                    bad_length = 0;
                    events += 1;
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
            if sys::display_blanked() {
                // The key only wakes the screen, it is not typed into the shell.
                sys::write(WAKE_SEQUENCE);
            } else {
                let _ = out.send(input_handle);
            }
        }
        if events > 0 {
            got_events = true;
            released = false;
            last_key = now();
            if let Some(brightness) = dimmed.take() {
                let _ = kbd::apply_led(bus, LedCommand::Brightness(brightness));
            }
        }
        if !drained {
            // More events may be queued: keep draining without waiting for the line.
            continue;
        }
        if !released {
            if kbd::Bus::transfer(bus, &[kbd::REG_INT_STA, 0], &mut []).is_err() {
                sys::write(b"[kbd] lost\n");
                return;
            }
            released = true;
            // Look once more: an event queued before the release would not raise INT again.
            continue;
        }
        // Sleep until the keyboard raises INT. While the LEDs are lit the wait is cut at the
        // idle deadline so they can be dimmed; once dimmed there is no timeout at all.
        let timeout = if dimmed.is_some() {
            0
        } else {
            IDLE_TICKS
                .saturating_sub(now().wrapping_sub(last_key))
                .clamp(1, MAX_WAIT_TICKS)
        };
        let rc = sys::event_wait(int_event, timeout);
        if rc == abi::ETIMEDOUT {
            if dimmed.is_none() && now().wrapping_sub(last_key) >= IDLE_TICKS {
                if let Ok(brightness) = kbd::read_brightness(bus) {
                    if brightness > 0 && kbd::apply_led(bus, LedCommand::Brightness(0)).is_ok() {
                        dimmed = Some(brightness);
                    }
                }
            }
            continue;
        }
        if rc != 0 {
            sys::write(b"[kbd] interrupt event lost\n");
            return;
        }
        let had_events = core::mem::replace(&mut got_events, false);
        released = false;
        if !had_events {
            // Woken with nothing to read (the line can rise a moment before the event is
            // readable): retry shortly rather than hammer the I2C bus.
            sys::sleep_ticks(WAKE_RETRY_TICKS);
        }
    }
}

fn main(input_handle: u32, i2c_ext1: u32, int_event: u32, _a3: u32, _a4: u32, _a5: u32) -> ! {
    let mut bus = SysBus(i2c_ext1);
    loop {
        if let Ok(f0) = kbd::init(&mut bus) {
            let mut line = TextBuf::<64>::new();
            line.append(b"[kbd] Tab5 Keyboard 0x6d reg_f0=");
            line.hex8(f0);
            line.append(b" character mode, INT on GPIO50\n");
            sys::write(line.as_slice());
            serve(&mut bus, input_handle, int_event);
        }
        sys::sleep_ticks(RECHECK_TICKS);
    }
}
