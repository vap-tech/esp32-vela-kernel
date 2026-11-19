#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{Buffer, arena::Arena, sys};

vela_user::entry!(main);

const FAILED: &[u8] = b"service unavailable\n";

#[derive(Clone, Copy)]
struct Caps {
    request: u32,
    spawn: u32,
    display: u32,
    sentinel: u32,
}

fn read_file(path: &[u8], request_handle: u32) {
    let mut request = Buffer::new();
    request.push(1);
    request.append(path);
    let mut response = Buffer::new();
    let result = sys::call6(
        abi::IPC_CALL,
        request_handle,
        request.ptr() as u32,
        request.len() as u32,
        response.mut_ptr() as u32,
        abi::IPC_BYTES as u32,
        100,
    )
    .0;
    if result == 0 || result > abi::IPC_BYTES as u32 {
        sys::write(FAILED);
        return;
    }
    response.set_len(result as usize);
    // Error replies carry userspace diagnostics, never fabricated file data.
    sys::write(&response.as_slice()[1..]);
}

fn arena_probe() -> bool {
    let Ok(mut arena) = Arena::new(4096) else {
        return false;
    };
    let layout = core::alloc::Layout::from_size_align(31, 64).unwrap();
    let first = arena.allocate(layout);
    let second = arena.allocate(layout);
    let mut ok = false;
    if let (Some(a), Some(b)) = (first, second) {
        // SAFETY: distinct live allocations, both at least 31 writable bytes.
        unsafe {
            a.as_ptr().write_volatile(0x5a);
            b.as_ptr().write_volatile(0xa5);
            ok = a.as_ptr().read_volatile() == 0x5a && b.as_ptr().read_volatile() == 0xa5;
        }
        ok &= a.as_ptr() as usize % 64 == 0 && b.as_ptr() as usize % 64 == 0;
    }
    ok &= arena
        .allocate(core::alloc::Layout::from_size_align(4096, 1).unwrap())
        .is_none();
    let released = arena.release().is_ok();
    ok && released && arena.allocate(layout).is_none()
}

fn sentinel_data(sentinel_cap: u32, sequence: u32, data: &mut Buffer) -> (u32, u32) {
    let mut result = (0, 0);
    for _ in 0..8 {
        result = sys::call6(
            abi::SENTINEL_LOG,
            sentinel_cap,
            sequence,
            data.mut_ptr() as u32,
            24,
            0,
            0,
        );
        if result.0 != (-16i32) as u32 {
            break;
        }
        sys::sleep(); // bounded retry if automatic heartbeat currently owns the request slot
    }
    if result.0 == 24 {
        data.set_len(24);
    }
    result
}

fn sentinel_log(sentinel_cap: u32) {
    let mut data = Buffer::new();
    let (result, notices) = sentinel_data(sentinel_cap, 0, &mut data);
    if result != 24 {
        sys::write(FAILED);
        return;
    }
    let oldest = data.word(4);
    let newest = data.word(8);
    let mut out = Buffer::new();
    out.append(b"Sentinel log epoch=");
    out.decimal(data.word(0));
    out.append(b" hz=");
    out.decimal(data.word(20));
    out.push(b'\n');
    sys::write(out.as_slice());

    out.clear();
    out.append(b"oldest=");
    out.decimal(oldest);
    out.append(b" newest=");
    out.decimal(newest);
    out.append(b" overwritten=");
    out.decimal(data.word(12));
    out.append(b" capacity=");
    out.decimal(data.word(16));
    out.push(b'\n');
    sys::write(out.as_slice());

    out.clear();
    out.append(b" diagnostic_notices=");
    out.decimal(notices);
    out.push(b'\n');
    sys::write(out.as_slice());

    // Bounded snapshot tail; new arrivals never make this an infinite dump.
    for offset in 0..16u32 {
        let sequence = oldest.wrapping_add(offset);
        if oldest == 0 || sequence == 0 || sequence > newest {
            break;
        }
        let result = sentinel_data(sentinel_cap, sequence, &mut data).0;
        if result != 24 {
            if result == (-75i32) as u32 {
                sys::write(b"record overwritten during read; retry log\n");
            } else {
                sys::write(FAILED);
            }
            break;
        }
        out.clear();
        out.push(b'#');
        out.decimal(data.word(0));
        out.append(b" lp_ticks=");
        out.decimal64(data.word(8), data.word(12));
        let event_name: &[u8] = match data.word(4) {
            1 => b" boot",
            2 => b" first_heartbeat",
            3 => b" timeout",
            4 => b" recovered",
            5 => b" hp_diagnostic",
            6 => b" wake_armed",
            7 => b" wake_fired",
            8 => b" wake_cancelled",
            _ => b" unknown",
        };
        out.append(event_name);
        out.append(b" a0=");
        out.decimal(data.word(16));
        out.append(b" a1=");
        out.decimal(data.word(20));
        out.push(b'\n');
        sys::write(out.as_slice());
    }
}

fn wake_command(line: &Buffer, sentinel_cap: u32) -> bool {
    const PREPARE_PREFIX: &[u8] = b"sentinel prepare ";
    const ARM_PREFIX: &[u8] = b"sentinel arm ";
    const WAKE_PREFIX: &[u8] = b"sentinel wake ";

    let wait = line.eq_at(0, b"sentinel wait");
    let cancel = line.eq_at(0, b"sentinel cancel");
    let prepare = line.starts_with(PREPARE_PREFIX);
    let arm = prepare || line.starts_with(ARM_PREFIX);
    let wake = line.starts_with(WAKE_PREFIX);
    if !wait && !cancel && !arm && !wake {
        return false;
    }
    let number = if cancel {
        abi::SENTINEL_CANCEL_WAKE
    } else if wait {
        abi::SENTINEL_WAIT_WAKE
    } else if prepare {
        abi::SENTINEL_PREPARE_SLEEP
    } else {
        abi::SENTINEL_ARM_WAKE
    };
    let argument = if arm || wake {
        let prefix_len = if prepare {
            PREPARE_PREFIX.len()
        } else if arm {
            ARM_PREFIX.len()
        } else {
            WAKE_PREFIX.len()
        };
        let Some(ms) = line.delay_ms(prefix_len) else {
            sys::write(b"wake delay must be 1..60000 ms\n");
            return true;
        };
        ms
    } else {
        0
    };
    let mut result = (0, 0);
    for _ in 0..8 {
        result = sys::call6(number, sentinel_cap, argument, 0, 0, 0, 0);
        if result.0 != (-16i32) as u32 {
            break;
        }
        sys::sleep();
    }
    if wake && (result.0 as i32) > 0 {
        result = sys::call6(abi::SENTINEL_WAIT_WAKE, sentinel_cap, result.0, 0, 0, 0, 0);
    }
    if (result.0 as i32) <= 0 {
        let message: &[u8] = match result.0 as i32 {
            -16 => b"wake busy; collect or cancel prior alarm\n",
            -2 => b"no pending wake\n",
            -114 => b"wake already fired; use sentinel wait\n",
            -110 => b"wake timeout; LP slot retained\n",
            _ => FAILED,
        };
        sys::write(message);
    } else if cancel {
        sys::write(b"Sentinel wake cancelled\n");
    } else {
        let mut out = Buffer::new();
        if prepare {
            out.append(b"Sentinel sleep prepared token=");
        } else if arm {
            out.append(b"Sentinel wake armed token=");
        } else {
            out.append(b"Sentinel WAKE_EVENT token=");
        }
        out.decimal(result.0);
        if !arm {
            out.append(b" elapsed_ticks=");
            out.decimal(result.1);
        }
        out.push(b'\n');
        sys::write(out.as_slice());
    }
    true
}

fn command(line: &Buffer, caps: Caps) {
    let text = line.as_slice();
    if text.is_empty() {
        return;
    }
    if wake_command(line, caps.sentinel) {
        return;
    }
    match text {
        b"help" => {
            sys::write(
                b"help | cat <path> | uptime | run hello | run pic | run dma | memtest | sentinel [hello/status/pause/resume/log/mark]\n",
            );
            sys::write(
                b"sentinel wake/arm/prepare <ms> | sentinel wait/cancel | display test/orbit/pixel/diag/boot\n",
            );
        }
        b"display test" | b"display orbit" | b"display pixel" | b"display diag"
        | b"display boot" => {
            let (mode, ok_msg): (u32, &[u8]) = match text {
                b"display orbit" => (2, b"Orbit scanout active\n"),
                b"display pixel" => (3, b"Pixel scanout active (360,640)\n"),
                b"display diag" => (4, b"Diagnostic scanout active\n"),
                b"display boot" => (5, b"Boot image scanout active\n"),
                _ => (1, b"ST7121 DSI bars enabled\n"),
            };
            let mut rc = sys::call(abi::DISPLAY_TEST, caps.display, mode, 0, 0);
            while (rc as i32) > 0 {
                sys::sleep_ticks(rc);
                rc = sys::call(abi::DISPLAY_TEST, caps.display, 0, 0, 0);
            }
            if rc == 0 {
                sys::write(ok_msg);
            } else {
                let mut out = Buffer::new();
                out.append(b"display error ");
                out.decimal(0u32.wrapping_sub(rc));
                out.push(b'\n');
                sys::write(out.as_slice());
            }
        }
        b"run hello" | b"run pic" | b"run dma" => {
            let selector = match text {
                b"run pic" => 1,
                b"run dma" => 2,
                _ => 0,
            };
            if (sys::call(abi::SPAWN_HELLO, caps.spawn, selector, 0, 0) as i32) < 0 {
                sys::write(FAILED);
            }
        }
        b"sentinel log" => sentinel_log(caps.sentinel),
        b"sentinel mark" => {
            let ticks = sys::call(abi::TICKS, 0, 0, 0, 0);
            let seq = sys::call(abi::SENTINEL_DIAG, caps.sentinel, 0x4d41_524b, ticks, 0);
            if (seq as i32) > 0 {
                let mut out = Buffer::new();
                out.append(b"Sentinel ACK seq ");
                out.decimal(seq);
                out.push(b'\n');
                sys::write(out.as_slice());
            } else {
                sys::write(FAILED);
            }
        }
        b"sentinel pause" | b"sentinel resume" => {
            let resume = text == b"sentinel resume";
            if sys::call(
                abi::SENTINEL_CONTROL,
                caps.sentinel,
                u32::from(resume),
                0,
                0,
            ) == 0
            {
                if resume {
                    sys::write(b"Sentinel heartbeat resumed\n");
                } else {
                    sys::write(b"Sentinel heartbeat paused\n");
                }
            } else {
                sys::write(FAILED);
            }
        }
        b"sentinel status" => {
            let mut data = Buffer::new();
            let (result, notices) = sys::call6(
                abi::SENTINEL_QUERY,
                caps.sentinel,
                data.mut_ptr() as u32,
                24,
                0,
                0,
                0,
            );
            if result == 24 {
                data.set_len(24);
                let mut out = Buffer::new();
                out.append(b"Sentinel state=");
                match data.word(0) {
                    0 => out.append(b"unarmed"),
                    1 => out.append(b"healthy"),
                    3 => out.append(b"prepared"),
                    _ => out.append(b"missed"),
                }
                out.append(b" age_ms=");
                out.decimal64(data.word(4), data.word(8));
                out.append(b" lp_hz=");
                out.decimal(data.word(12));
                out.append(b" beats=");
                out.decimal(data.word(16));
                out.append(b" missed=");
                out.decimal(data.word(20));
                out.append(b" notices=");
                out.decimal(notices);
                out.push(b'\n');
                sys::write(out.as_slice());
            } else {
                sys::write(FAILED);
            }
        }
        b"sentinel hello" => {
            let seq = sys::call(abi::SENTINEL_HELLO, caps.sentinel, 0, 0, 0);
            if (seq as i32) > 0 {
                let mut out = Buffer::new();
                out.append(b"Sentinel ACK seq ");
                out.decimal(seq);
                out.push(b'\n');
                sys::write(out.as_slice());
            } else {
                sys::write(FAILED);
            }
        }
        b"sentinel" => {
            let (version, epoch) = sys::call6(abi::SENTINEL_STATUS, caps.sentinel, 0, 0, 0, 0, 0);
            let mut out = Buffer::new();
            if version == (-110i32) as u32 {
                out.append(b"Sentinel heartbeat timeout, epoch ");
            } else if version != 0 {
                out.append(b"Sentinel READY v");
                out.decimal(version);
                out.append(b", epoch ");
            } else {
                out.append(b"Sentinel pending, epoch ");
            }
            out.decimal(epoch);
            out.push(b'\n');
            sys::write(out.as_slice());
        }
        b"memtest" => {
            if arena_probe() {
                sys::write(b"arena: aligned allocations, bounds, release OK\n");
            } else {
                sys::write(b"arena: FAIL\n");
            }
        }
        b"uptime" => read_file(b"/proc/uptime", caps.request),
        _ if text.len() > 4 && text.starts_with(b"cat ") => {
            read_file(&text[4..], caps.request);
        }
        _ => sys::write(b"unknown command (try help)\n"),
    }
}

fn main(
    _reply_handle: u32,
    request_handle: u32,
    input_handle: u32,
    spawn_cap: u32,
    display_cap: u32,
    sentinel_cap: u32,
) -> ! {
    let caps = Caps {
        request: request_handle,
        spawn: spawn_cap,
        display: display_cap,
        sentinel: sentinel_cap,
    };
    let mut line = Buffer::new();
    let mut skip_lf = false;
    let mut overflow = false;
    sys::write(b"vela:/$ ");
    loop {
        let mut input = Buffer::new();
        if !input.receive(input_handle) {
            sys::exit();
        }
        for i in 0..input.len() {
            let byte = input.get(i);
            if byte == b'\n' && skip_lf {
                skip_lf = false;
                continue;
            }
            skip_lf = byte == b'\r';
            if byte == b'\r' || byte == b'\n' {
                sys::write(b"\n");
                if overflow {
                    sys::write(b"line too long\n");
                } else {
                    command(&line, caps);
                }
                line.clear();
                overflow = false;
                sys::write(b"vela:/$ ");
            } else if byte == 8 || byte == 127 {
                if line.pop() {
                    sys::write(b"\x08 \x08");
                }
            } else if (32..127).contains(&byte) {
                if line.len() < 80 {
                    line.push(byte);
                    sys::write(core::slice::from_ref(&byte));
                } else {
                    overflow = true;
                }
            }
        }
    }
}
