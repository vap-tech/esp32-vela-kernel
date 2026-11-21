#![no_std]
#![no_main]

//! Framebuffer console: owns the display framebuffer and mirrors the system console
//! (kernel log and everything services write) onto the Tab5 screen.

use vela_abi::syscall as abi;
use vela_user::{
    TextBuf,
    arena::Arena,
    fbcon::{Console, FB_WORDS, IDLE_TICKS},
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
fn bring_up(display: u32) -> Result<usize, u32> {
    let mut fb = None;
    let mut calls = 1;
    let mut rc = sys::call(abi::DISPLAY_TEST, display, abi::DISPLAY_MODE_CONSOLE, 0, 0);
    while (rc as i32) > 0 {
        sys::sleep_ticks(rc);
        let mut fb_base = 0;
        if calls == abi::DISPLAY_CONSOLE_PREP_CALLS {
            // CPLL is locked now; map the framebuffer under the final clock.
            match Arena::new_psram(abi::DISPLAY_FB_BYTES) {
                Ok(arena) => {
                    fb_base = arena.base() as u32;
                    fb = Some(arena.base());
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
        fb.ok_or(abi::EINVAL)
    } else {
        Err(rc)
    }
}

fn main(display: u32, log_handle: u32, _a2: u32, _a3: u32, _a4: u32, _a5: u32) -> ! {
    let fb = match bring_up(display) {
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
    log(b"[fbcon] console online 1280x720\n");

    // SAFETY: single-threaded service; this is the only reference to `CONSOLE`.
    let console = unsafe { &mut *core::ptr::addr_of_mut!(CONSOLE) };
    let mut cursor = 0u32;
    let mut buffer = [0u8; 128];
    let mut measured = false;
    let mut last_activity = now();
    let mut blanked = false;
    loop {
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
        let start = sys::rdcycle64();
        // SAFETY: `fb` is the 2 MiB PSRAM region we own, at least `FB_WORDS` words long.
        let drawn = unsafe { console.render(fb) };
        if !measured && drawn > 100 {
            measured = true;
            let kcycles = ((sys::rdcycle64() - start) / 1000) as u32;
            let mut line = TextBuf::<96>::new();
            line.append(b"[fbcon] first render ");
            line.decimal(drawn as u32);
            line.append(b" cells, ");
            line.decimal(kcycles);
            line.append(b" kcycles, fb words ");
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
