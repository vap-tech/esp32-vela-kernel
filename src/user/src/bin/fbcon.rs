#![no_std]
#![no_main]

//! Framebuffer console: owns the display framebuffer and mirrors the system console
//! (kernel log and everything services write) onto the Tab5 screen.

use vela_abi::syscall as abi;
use vela_user::{
    TextBuf,
    arena::Arena,
    fbcon::{COLS, Console, FB_WORDS, IDLE_TICKS, PALETTE, draw_cell},
    sys,
};

vela_user::entry!(main);

/// The text grid is about 5 KiB, more than the 4 KiB process stack, so it lives in `.bss`.
static mut CONSOLE: Console = Console::new();

/// Console output is polled; one tick is 10 ms.
const POLL_TICKS: u32 = 2;
/// Log polling period while the backlight is off (10 ms ticks).
const BLANKED_POLL_TICKS: u32 = 10;

/// Backlight level restored on wake (matches the kernel's boot default).
const BACKLIGHT_ON: u32 = 160;

fn now() -> u32 {
    sys::call(abi::TICKS, 0, 0, 0, 0)
}

fn set_backlight(display: u32, level: u32) {
    let _ = sys::call(
        abi::DISPLAY_TEST,
        display,
        abi::DISPLAY_MODE_BACKLIGHT,
        level,
        0,
    );
}

fn log(text: &[u8]) {
    sys::write(text);
}

fn log_number(prefix: &[u8], value: u32, suffix: &[u8]) {
    let mut line = TextBuf::<96>::new();
    line.append(prefix);
    line.decimal(value);
    line.append(suffix);
    sys::write(line.as_slice());
}

/// Bring the panel up with a framebuffer we own. Returns its base address.
fn bring_up(display: u32, splash: bool) -> Result<usize, u32> {
    let mut fb = None;
    let mut calls = 1;
    let mut rc = sys::call(
        abi::DISPLAY_TEST,
        display,
        abi::DISPLAY_MODE_CONSOLE,
        u32::from(splash),
        0,
    );
    while (rc as i32) > 0 {
        sys::sleep_ticks(rc);
        let mut fb_base = 0;
        if calls == abi::DISPLAY_CONSOLE_PREP_CALLS {
            // CPLL is locked now; map the framebuffer under the final clock.
            match Arena::new_psram(abi::DISPLAY_FB_BYTES) {
                Ok(arena) => {
                    fb_base = arena.base() as u32;
                    fb = Some(arena);
                }
                Err(code) => {
                    let _ = sys::call(abi::DISPLAY_TEST, display, abi::DISPLAY_MODE_OFF, 0, 0);
                    return Err(code);
                }
            }
        }
        rc = sys::call(
            abi::DISPLAY_TEST,
            display,
            abi::DISPLAY_MODE_STEP,
            fb_base,
            0,
        );
        calls += 1;
    }
    if rc == 0 {
        fb.map(|arena| arena.base()).ok_or(abi::EINVAL)
    } else {
        if let Some(mut arena) = fb {
            let _ = arena.release();
        }
        Err(rc)
    }
}

/// SAFETY: `fb` points to this service's complete, writable framebuffer.
unsafe fn render_export(fb: *mut u32, writable: bool) {
    let mode: &[u8] = if writable {
        b"Read and write"
    } else {
        b"Read only"
    };
    let lines: &[(usize, &[u8], u16)] = &[
        (8, b"USB SD export", PALETTE[14]),
        (10, mode, PALETTE[15]),
        (
            12,
            b"Eject the disk on your computer to return.",
            PALETTE[7],
        ),
    ];
    unsafe { fb.write_bytes(0, FB_WORDS) };
    for &(row, text, color) in lines {
        let left = (COLS - text.len()) / 2;
        for (i, &ch) in text.iter().enumerate() {
            unsafe { draw_cell(fb, left + i, row, ch, color, false) };
        }
    }
}

fn scan_counts() -> [u32; 3] {
    let mut b = [0u8; abi::STATS_STREAMS_BYTES];
    assert_eq!(
        sys::call(
            abi::STATS,
            b.as_mut_ptr() as u32,
            b.len() as u32,
            abi::STATS_STREAMS,
            0
        ),
        0
    );
    [
        vela_user::word_le(&b, 4),
        vela_user::word_le(&b, 8),
        vela_user::word_le(&b, 12),
    ]
}
fn main(display: u32, log_handle: u32, _a2: u32, _a3: u32, _a4: u32, _a5: u32) -> ! {
    let fb = match bring_up(display, true).or_else(|_| bring_up(display, false)) {
        Ok(fb) => fb as *mut u32,
        Err(code) => {
            log_number(
                b"[fbcon] display bring-up failed: ",
                0u32.wrapping_sub(code),
                b"\n",
            );
            sys::process_exit();
        }
    };
    let mut first_frame = true;
    log(b"[fbcon] console online 1280x720\n");

    // SAFETY: single-threaded service; this is the only reference to `CONSOLE`.
    let console = unsafe { &mut *core::ptr::addr_of_mut!(CONSOLE) };
    let mut cursor = 0u32;
    let mut buffer = [0u8; 128];
    let mut measured = false;
    let mut last_activity = now();
    let mut blanked = false;
    let mut showing_export = false;
    loop {
        let flags = sys::display_flags();
        if flags & abi::DISPLAY_FLAG_SD_EXPORT != 0 {
            if !showing_export {
                // SAFETY: `fb` is still the console's owned scanout buffer.
                unsafe { render_export(fb, flags & abi::DISPLAY_FLAG_SD_EXPORT_RW != 0) };
                showing_export = true;
                if blanked {
                    set_backlight(display, BACKLIGHT_ON);
                    blanked = false;
                }
            }
            sys::sleep_ticks(POLL_TICKS);
            continue;
        }
        if showing_export {
            // SAFETY: clear the overlay, then let the saved console grid redraw.
            unsafe { fb.write_bytes(0, FB_WORDS) };
            console.framebuffer_cleared();
            last_activity = now();
            showing_export = false;
        }
        let mut active = false;
        loop {
            let (count, next) = sys::console_log(log_handle, &mut buffer, cursor);
            if count == 0 {
                break;
            }
            console.write(&buffer[..count]);
            cursor = next;
            active = true;
        }
        let start = now();
        // SAFETY: `fb` is the 2 MiB PSRAM region we own, at least `FB_WORDS` words long.
        let drawn = if first_frame {
            first_frame = false;
            {
                let a = scan_counts();
                let n = unsafe { console.replace_artwork(fb) };
                let elapsed = now().wrapping_sub(start);
                let b = scan_counts();
                let mut l = TextBuf::<96>::new();
                l.append(b"[handoff] ticks frames underruns dma_errors ");
                l.decimal(elapsed);
                for i in 0..3 {
                    l.push(32);
                    l.decimal(b[i].wrapping_sub(a[i]));
                }
                l.push(10);
                sys::write(l.as_slice());
                n
            }
        } else {
            unsafe { console.render(fb) }
        };
        if !measured && drawn > 100 {
            measured = true;
            let ticks = now().wrapping_sub(start);
            let mut line = TextBuf::<96>::new();
            line.append(b"[fbcon] first render ");
            line.decimal(drawn as u32);
            line.append(b" cells, ");
            line.decimal(ticks);
            line.append(b" ticks (10 ms), fb words ");
            line.decimal(FB_WORDS as u32);
            line.push(b'\n');
            sys::write(line.as_slice());
        }
        if active {
            last_activity = now();
            if blanked {
                // The frame is already drawn; light the panel only now.
                set_backlight(display, BACKLIGHT_ON);
                blanked = false;
            }
        } else if !blanked && now().wrapping_sub(last_activity) >= IDLE_TICKS {
            set_backlight(display, 0);
            blanked = true;
        }
        // Nothing is drawn while the panel is dark, so look at the log less often.
        sys::sleep_ticks(if blanked {
            BLANKED_POLL_TICKS
        } else {
            POLL_TICKS
        });
    }
}
