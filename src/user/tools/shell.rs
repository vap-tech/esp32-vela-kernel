#![no_std]
#![no_main]

use vela_abi::syscall::{self as abi, SpawnCap, SpawnCaps};
use vela_user::{
    Buffer, TextBuf,
    file::{STATUS_NOENT, fs_load_arena, fs_mkdir, fs_unlink, fs_unmount, fs_write, fs_write_bulk},
    io, sys,
    term::EscapeFilter,
};

vela_user::entry!(main);

/// Bold green; the trailing space stays inside the colour so `vela:/$ ` is still a plain substring.
const PROMPT: &[u8] = b"\x1b[1;32mvela:/$ \x1b[0m";

/// Staging buffer for streaming `cmd > /path` output into bulk writes (kept out of stack).
static mut REDIR_BUF: [u8; 2048] = [0; 2048];

#[derive(Clone, Copy)]
struct Caps {
    vfs: u32,
    input: u32,
    spawn: u32,
    display: u32,
}

#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn spawn_program(
    caps: Caps,
    path: &[u8],
    argument: &[u8],
    stdin_handle: u32,
    stdout_handle: u32,
    implicit_cmd: bool,
    background_driver: bool,
    wait: bool,
) -> Option<u32> {
    if argument.len() > abi::SPAWN_ARG_MAX {
        sys::write(b"run: argument too long\n");
        return None;
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
            return None;
        }
    };
    // Uniform child capability layout:
    //   cap0 (a0) = vfs
    //   cap1 (a3) = stdin  (optional CAP_RECEIVE stream)
    //   cap2 (a4) = stdout (optional CAP_SEND stream; 0 = kernel console)
    //   cap3 (a5) = extra  (display CAP_CONTROL for sdexport.elf)
    let mut spawn_caps = SpawnCaps::EMPTY;
    spawn_caps.count = 4;
    spawn_caps.caps[0] = SpawnCap {
        handle: caps.vfs,
        rights: if background_driver {
            abi::CAP_SEND | abi::CAP_ADMIN
        } else {
            abi::CAP_SEND
        },
    };
    if stdin_handle != 0 {
        spawn_caps.caps[1] = SpawnCap {
            handle: stdin_handle,
            rights: abi::CAP_RECEIVE,
        };
    }
    if stdout_handle != 0 {
        spawn_caps.caps[2] = SpawnCap {
            handle: stdout_handle,
            rights: abi::CAP_SEND,
        };
    }
    if path == b"/sd/bin/sdexport.elf" && caps.display != 0 {
        spawn_caps.caps[3] = SpawnCap {
            handle: caps.display,
            rights: abi::CAP_CONTROL,
        };
    }
    let syscall_num = if wait {
        abi::SPAWN_IMAGE_WAIT
    } else {
        abi::SPAWN_IMAGE
    };
    let (pid, _) = sys::call6(
        syscall_num,
        caps.spawn,
        arena.base() as u32,
        len as u32,
        argument.as_ptr() as u32,
        argument.len() as u32,
        &raw const spawn_caps as u32,
    );
    let _ = arena.release();
    if (pid as i32) < 0 {
        sys::write(b"run: ELF rejected or process slots exhausted\n");
        None
    } else {
        if background_driver {
            let mut out = TextBuf::<64>::new();
            out.append(b"started pid=");
            out.decimal(pid);
            out.push(b'\n');
            sys::write(out.as_slice());
        }
        Some(pid)
    }
}

#[inline(never)]
fn drain_stream_to_file(vfs: u32, pipe: u32, path: &[u8]) {
    // SAFETY: shell is single-threaded and never re-enters `drain_stream_to_file`.
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(REDIR_BUF) };
    let mut used = 0usize;
    let mut offset = 0u32;
    let mut response = Buffer::new();
    let mut write_failed = false;
    let mut wrote_any = false;
    let mut chunk = [0u8; abi::IPC_BYTES];

    loop {
        let (res, _) = sys::call6(
            abi::RECV,
            pipe,
            chunk.as_mut_ptr() as u32,
            chunk.len() as u32,
            0,
            0,
            0,
        );
        if (res as i32) <= 0 || res as usize > chunk.len() {
            break;
        }
        let n = res as usize;
        if write_failed {
            continue;
        }
        if used + n > buf.len() {
            if fs_write_bulk(vfs, path, offset, &buf[..used], &mut response).is_err() {
                write_failed = true;
                continue;
            }
            offset = offset.saturating_add(used as u32);
            used = 0;
            wrote_any = true;
        }
        buf[used..used + n].copy_from_slice(&chunk[..n]);
        used += n;
    }

    if !write_failed && (used > 0 || !wrote_any) {
        let _ = fs_write_bulk(vfs, path, offset, &buf[..used], &mut response);
    }
}

#[inline(never)]
fn echo_to_file(text: &[u8], path: &[u8], vfs: u32) {
    let mut payload = Buffer::new();
    payload.append(text);
    payload.push(b'\n');
    let mut response = Buffer::new();
    if let Ok(len) = fs_write(vfs, path, 0, payload.as_slice(), &mut response)
        && len > 0
    {
        sys::write(&response.as_slice()[1..]);
    }
}

#[inline(never)]
fn echo_to_pipe(text: &[u8], pipe: u32) -> bool {
    let mut payload = Buffer::new();
    payload.append(text);
    payload.push(b'\n');
    if io::send_stream(pipe, payload.as_slice()).is_err() {
        return false;
    }
    // Send explicit 0-byte EOF frame so the consumer terminates cleanly.
    sys::call(abi::SEND, pipe, payload.ptr() as u32, 0, 0) == 0
}

#[inline(never)]
fn rm_command(path: &[u8], vfs: u32) {
    if !path.starts_with(b"/") {
        sys::write(b"usage: rm </path>\n");
        return;
    }
    let mut response = Buffer::new();
    if fs_unlink(vfs, path, &mut response).is_err() && response.len() > 1 {
        sys::write(&response.as_slice()[1..]);
    }
}

#[inline(never)]
fn mkdir_command(path: &[u8], vfs: u32) {
    if !path.starts_with(b"/") {
        sys::write(b"usage: mkdir </path>\n");
        return;
    }
    let mut response = Buffer::new();
    if fs_mkdir(vfs, path, &mut response).is_err() && response.len() > 1 {
        sys::write(&response.as_slice()[1..]);
    }
}

#[inline(never)]
fn umount_command(prefix: &[u8], vfs: u32) {
    if !prefix.starts_with(b"/") {
        sys::write(b"usage: umount </mount/prefix>\n");
        return;
    }
    let mut response = Buffer::new();
    if fs_unmount(vfs, prefix, &mut response).is_err() && response.len() > 1 {
        sys::write(&response.as_slice()[1..]);
    }
}

fn is_tool_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Resolve a command stage into `(elf_path, arg, implicit_cmd, pass_interactive_stdin)`.
fn resolve_external<'a>(
    stage: &'a [u8],
    path_buf: &'a mut Buffer,
) -> Option<(&'a [u8], &'a [u8], bool, bool)> {
    let mut words = stage.splitn(2, |&b| b == b' ');
    let cmd = words.next().unwrap_or_default();
    let arg = words.next().unwrap_or_default().trim_ascii();
    if cmd == b"run" {
        let mut parts = arg.splitn(2, |&b| b == b' ');
        let path = parts.next().unwrap_or_default();
        let prog_arg = parts.next().unwrap_or_default().trim_ascii();
        if path.starts_with(b"/") {
            Some((path, prog_arg, false, true))
        } else {
            sys::write(b"usage: run </path/to/app.elf> [arg]\n");
            None
        }
    } else if is_tool_name(cmd) {
        path_buf.clear();
        path_buf.append(b"/sd/bin/");
        path_buf.append(cmd);
        path_buf.append(b".elf");
        Some((path_buf.as_slice(), arg, true, false))
    } else {
        sys::write(b"\x1b[31munknown command (try help)\x1b[0m\n");
        None
    }
}

#[inline(never)]
fn run_pipeline(left: &[u8], right: &[u8], out_path: Option<&[u8]>, caps: Caps) {
    let Ok(mid_pipe) = sys::endpoint_create_stream() else {
        sys::write(b"pipe: endpoint table full\n");
        return;
    };
    let out_pipe = if out_path.is_some() {
        match sys::endpoint_create_stream() {
            Ok(ep) => ep,
            Err(_) => {
                let _ = sys::endpoint_close(mid_pipe);
                sys::write(b"redirect: endpoint table full\n");
                return;
            }
        }
    } else {
        0
    };

    // Start left stage (either `echo` builtin or external ELF writing to `mid_pipe`).
    let mut left_words = left.splitn(2, |&b| b == b' ');
    let left_cmd = left_words.next().unwrap_or_default();
    let left_arg = left_words.next().unwrap_or_default().trim_ascii();
    let left_ok = if left_cmd == b"echo" {
        echo_to_pipe(left_arg, mid_pipe)
    } else {
        let mut path_buf = Buffer::new();
        if let Some((path, arg, implicit, _)) = resolve_external(left, &mut path_buf) {
            spawn_program(caps, path, arg, 0, mid_pipe, implicit, false, false).is_some()
        } else {
            false
        }
    };

    if left_ok {
        let mut path_buf = Buffer::new();
        if let Some((path, arg, implicit, _)) = resolve_external(right, &mut path_buf) {
            let wait_right = out_pipe == 0;
            if spawn_program(
                caps, path, arg, mid_pipe, out_pipe, implicit, false, wait_right,
            )
            .is_some()
                && let Some(dest) = out_path
            {
                drain_stream_to_file(caps.vfs, out_pipe, dest);
            }
        }
    }

    if out_pipe != 0 {
        let _ = sys::endpoint_close(out_pipe);
    }
    let _ = sys::endpoint_close(mid_pipe);
}

#[inline(never)]
fn run_single(stage: &[u8], out_path: Option<&[u8]>, caps: Caps) {
    let mut words = stage.splitn(2, |&b| b == b' ');
    let cmd = words.next().unwrap_or_default();
    let arg = words.next().unwrap_or_default().trim_ascii();

    match cmd {
        b"help" => {
            sys::write(
                b"\x1b[96mhelp\x1b[0m | \x1b[96mls\x1b[0m [path] | \x1b[96mcat\x1b[0m [path] | \x1b[96mwc\x1b[0m [path] | \x1b[96mecho\x1b[0m <val> | \x1b[96mmkdir\x1b[0m <path> | \x1b[96mrm\x1b[0m <path>\n\x1b[96mrun\x1b[0m <elf> [arg] | \x1b[96mstart\x1b[0m <drv.elf> [arg] | \x1b[96mumount\x1b[0m <path> | \x1b[96msdexport\x1b[0m [s|rw] | \x1b[96mexit\x1b[0m\nStreams: <cmd1> | <cmd2> [> <path>]   <cmd> > <path>   (writable /sd, /dev, /mnt)\n",
            );
        }
        b"echo" => {
            if let Some(path) = out_path {
                echo_to_file(arg, path, caps.vfs);
            } else {
                sys::write(arg);
                sys::write(b"\n");
            }
        }
        b"rm" => rm_command(arg, caps.vfs),
        b"mkdir" => mkdir_command(arg, caps.vfs),
        b"umount" => umount_command(arg, caps.vfs),
        b"exit" => sys::process_exit(),
        b"start" => {
            let mut parts = arg.splitn(2, |&b| b == b' ');
            let path = parts.next().unwrap_or_default();
            let prog_arg = parts.next().unwrap_or_default().trim_ascii();
            if path.starts_with(b"/") {
                let _ = spawn_program(caps, path, prog_arg, 0, 0, false, true, false);
            } else {
                sys::write(b"usage: start </path/to/driver.elf> [arg]\n");
            }
        }
        _ => {
            let mut path_buf = Buffer::new();
            let Some((path, prog_arg, implicit, interactive)) =
                resolve_external(stage, &mut path_buf)
            else {
                return;
            };
            if let Some(dest) = out_path {
                let Ok(out_pipe) = sys::endpoint_create_stream() else {
                    sys::write(b"redirect: endpoint table full\n");
                    return;
                };
                if spawn_program(caps, path, prog_arg, 0, out_pipe, implicit, false, false)
                    .is_some()
                {
                    drain_stream_to_file(caps.vfs, out_pipe, dest);
                }
                let _ = sys::endpoint_close(out_pipe);
            } else {
                let stdin = if interactive { caps.input } else { 0 };
                let _ = spawn_program(caps, path, prog_arg, stdin, 0, implicit, false, true);
            }
        }
    }
}

fn command(line: &Buffer, caps: Caps) {
    let text = line.as_slice().trim_ascii();
    if text.is_empty() {
        return;
    }
    let (pipeline, out_path) = if let Some(pos) = text.iter().rposition(|&b| b == b'>') {
        let lhs = text[..pos].trim_ascii();
        let rhs = text[pos + 1..].trim_ascii();
        if lhs.is_empty() || !rhs.starts_with(b"/") {
            sys::write(b"usage: <cmd> > </path>\n");
            return;
        }
        (lhs, Some(rhs))
    } else {
        (text, None)
    };

    if let Some(pos) = pipeline.iter().position(|&b| b == b'|') {
        let left = pipeline[..pos].trim_ascii();
        let right = pipeline[pos + 1..].trim_ascii();
        if left.is_empty() || right.is_empty() || right.contains(&b'|') {
            sys::write(b"usage: <cmd1> | <cmd2> [> </path>]\n");
            return;
        }
        run_pipeline(left, right, out_path, caps);
    } else {
        run_single(pipeline, out_path, caps);
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
        input: input_handle,
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
