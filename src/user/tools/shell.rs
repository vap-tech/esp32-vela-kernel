#![no_std]
#![no_main]

use vela_abi::syscall::{self as abi, SpawnCap, SpawnCaps};
use vela_user::{
    Buffer,
    file::{STATUS_NOENT, fs_load_arena, fs_write},
    sys,
    term::EscapeFilter,
};

vela_user::entry!(main);

/// Bold green; the trailing space stays inside the colour so `vela:/$ ` is still a plain substring.
const PROMPT: &[u8] = b"\x1b[1;32mvela:/$ \x1b[0m";

#[derive(Clone, Copy)]
struct Caps {
    vfs: u32,
    spawn: u32,
    display: u32,
}

#[inline(never)]
fn run_program(caps: Caps, path: &[u8], argument: &[u8], implicit_cmd: bool) {
    if argument.len() > abi::SPAWN_ARG_MAX {
        sys::write(b"run: argument too long\n");
        return;
    }
    let mut response = Buffer::new();
    let (mut arena, len) = match fs_load_arena(caps.vfs, path, abi::SPAWN_IMAGE_MAX, &mut response)
    {
        Ok(loaded) => loaded,
        Err(status) => {
            if implicit_cmd && status == STATUS_NOENT {
                sys::write(b"\x1b[31munknown command (try help)\x1b[0m\n");
            } else if response.len() > 1 {
                sys::write(&response.as_slice()[1..]);
            }
            return;
        }
    };
    let mut spawn_caps = SpawnCaps::EMPTY;
    spawn_caps.count = 1;
    spawn_caps.caps[0] = SpawnCap {
        handle: caps.vfs,
        rights: abi::CAP_SEND,
    };
    if path == b"/sd/bin/sdexport.elf" && caps.display != 0 {
        spawn_caps.count = 2;
        spawn_caps.caps[1] = SpawnCap {
            handle: caps.display,
            rights: abi::CAP_CONTROL,
        };
    }
    let (pid, _) = sys::call6(
        abi::SPAWN_IMAGE_WAIT,
        caps.spawn,
        arena.base() as u32,
        len as u32,
        argument.as_ptr() as u32,
        argument.len() as u32,
        &raw const spawn_caps as u32,
    );
    if (pid as i32) < 0 {
        sys::write(b"run: ELF rejected or process slots exhausted\n");
    }
    let _ = arena.release();
}

#[inline(never)]
fn echo_command(rest: &[u8], vfs: u32) {
    if let Some(pos) = rest.iter().rposition(|&b| b == b'>') {
        let data = rest[..pos].trim_ascii();
        let path = rest[pos + 1..].trim_ascii();
        if path.is_empty() {
            sys::write(b"usage: echo <text> > </path>\n");
            return;
        }
        let mut response = Buffer::new();
        if let Ok(len) = fs_write(vfs, path, 0, data, &mut response)
            && len > 0
        {
            sys::write(&response.as_slice()[1..]);
        }
    } else {
        sys::write(rest);
        sys::write(b"\n");
    }
}

fn is_tool_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn command(line: &Buffer, caps: Caps) {
    let text = line.as_slice().trim_ascii();
    if text.is_empty() {
        return;
    }
    let mut words = text.splitn(2, |&b| b == b' ');
    let cmd = words.next().unwrap_or_default();
    let arg = words.next().unwrap_or_default().trim_ascii();

    match cmd {
        b"help" => {
            sys::write(
                b"\x1b[96mhelp\x1b[0m | \x1b[96mls\x1b[0m [path] | \x1b[96mcat\x1b[0m <path> | \x1b[96mecho\x1b[0m <val> [> <path>] | \x1b[96mrun\x1b[0m <elf> [arg] | \x1b[96msdexport\x1b[0m [s|rw] | \x1b[96mexit\x1b[0m\nPaths: /proc  /sys (power, freq, display, sentinel/*)  /dev (i2c-*, kbdled, random)  /sd\n",
            );
        }
        b"echo" => echo_command(arg, caps.vfs),
        b"exit" => sys::process_exit(),
        b"run" => {
            let mut parts = arg.splitn(2, |&b| b == b' ');
            let path = parts.next().unwrap_or_default();
            let prog_arg = parts.next().unwrap_or_default().trim_ascii();
            if path.starts_with(b"/") {
                run_program(caps, path, prog_arg, false);
            } else {
                sys::write(b"usage: run </path/to/app.elf> [arg]\n");
            }
        }
        _ if is_tool_name(cmd) => {
            let mut path = Buffer::new();
            path.append(b"/sd/bin/");
            path.append(cmd);
            path.append(b".elf");
            run_program(caps, path.as_slice(), arg, true);
        }
        _ => sys::write(b"\x1b[31munknown command (try help)\x1b[0m\n"),
    }
}

fn main(
    vfs_handle: u32,
    _arg: u32,
    _len: u32,
    input_handle: u32,
    spawn_cap: u32,
    display_cap: u32,
) -> ! {
    if vfs_handle == 0 || input_handle == 0 || spawn_cap == 0 {
        sys::write(b"shell: missing required capabilities\n");
        sys::process_exit();
    }
    let caps = Caps {
        vfs: vfs_handle,
        spawn: spawn_cap,
        display: display_cap,
    };
    let mut line = Buffer::new();
    let mut skip_lf = false;
    let mut overflow = false;
    let mut escapes = EscapeFilter::new();
    sys::write(PROMPT);
    loop {
        let mut input = Buffer::new();
        if !input.receive(input_handle) {
            sys::process_exit();
        }
        for i in 0..input.len() {
            let byte = input.get(i);
            if escapes.swallow(byte) {
                continue;
            }
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
                sys::write(PROMPT);
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
