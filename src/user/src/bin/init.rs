#![no_std]
#![no_main]
//! Minimal flash `init` process.
//!
//! Reads the board's flash boot config (`boards/<board>/init.conf`), optionally loads a
//! runtime override file through VFS (e.g. `/sd/etc/init` on Tab5), and launches the target
//! ELF via `SPAWN_IMAGE_WAIT`. Contains no hardcoded storage assumptions so small boards
//! (ESP32-C3/C5/C6) can point `exec` directly at a flash/ROM image.

use vela_abi::syscall::{self as abi, SpawnCap, SpawnCaps};
use vela_user::{
    Buffer, boot_cfg,
    file::{fs_load_arena, fs_read_raw},
    sys,
};

vela_user::entry!(main);

const DEFAULT_BOARD_INIT: &[u8] = include_bytes!("../../../../boards/tab5/init.conf");
const BOARD_INIT: &[u8] = match option_env!("VELA_BOARD_INIT") {
    Some(text) => text.as_bytes(),
    None => DEFAULT_BOARD_INIT,
};

#[derive(Clone, Copy)]
struct Caps {
    vfs: u32,
    input: u32,
    spawn: u32,
    display: u32,
}

fn load_and_run(caps: Caps, path: &[u8], arg: &[u8]) -> Result<(), ()> {
    if arg.len() > abi::SPAWN_ARG_MAX {
        return Err(());
    }
    let mut response = Buffer::new();
    let Ok((mut arena, len)) = fs_load_arena(caps.vfs, path, abi::SPAWN_IMAGE_MAX, &mut response)
    else {
        return Err(());
    };

    let spawn_caps = SpawnCaps {
        count: 4,
        caps: [
            SpawnCap {
                handle: caps.vfs,
                rights: abi::CAP_SEND | abi::CAP_ADMIN | abi::CAP_PASS_ON_SPAWN,
            },
            SpawnCap {
                handle: caps.input,
                rights: abi::CAP_RECEIVE | abi::CAP_PASS_ON_SPAWN,
            },
            SpawnCap {
                handle: caps.spawn,
                rights: abi::CAP_EXECUTE,
            },
            SpawnCap {
                handle: caps.display,
                rights: abi::CAP_CONTROL | abi::CAP_PASS_ON_SPAWN,
            },
        ],
    };
    let (pid, _) = sys::call6(
        abi::SPAWN_IMAGE_WAIT,
        caps.spawn,
        arena.base() as u32,
        len as u32,
        arg.as_ptr() as u32,
        arg.len() as u32,
        &raw const spawn_caps as u32,
    );
    let _ = arena.release();
    if (pid as i32) < 0 { Err(()) } else { Ok(()) }
}

fn main(vfs: u32, input: u32, spawn: u32, display: u32, _a4: u32, _a5: u32) -> ! {
    let caps = Caps {
        vfs,
        input,
        spawn,
        display,
    };
    let board = boot_cfg::parse_board_config(BOARD_INIT);
    let mut alerted = false;
    let mut cfg_buf = Buffer::new();

    loop {
        let mut target_path = board.exec_path;
        let mut target_arg = board.exec_arg;

        if let Some(cfg_path) = board.config_path
            && let Ok(n) = fs_read_raw(caps.vfs, cfg_path, 0, &mut cfg_buf)
            && n > 0
            && let Some((path, arg)) = boot_cfg::parse_runtime_init(&cfg_buf.as_slice()[1..])
        {
            target_path = Some(path);
            target_arg = arg;
        }

        let Some(path) = target_path else {
            if !alerted {
                sys::write(b"[init] ALERT: no exec target in board boot config\n");
                alerted = true;
            }
            sys::sleep_ticks(200);
            continue;
        };

        if load_and_run(caps, path, target_arg).is_ok() {
            alerted = false;
            sys::write(b"[init] process exited, restarting...\n");
            sys::sleep_ticks(50);
        } else {
            if !alerted {
                sys::write(b"[init] ALERT: cannot load ");
                sys::write(path);
                sys::write(b" -- prepare media with /bin utilities (shell.elf, ls.elf, cat.elf)\n");
                alerted = true;
            }
            sys::sleep_ticks(150);
        }
    }
}
