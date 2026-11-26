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

extern "C" fn supervise_fbcon(handle: u32) -> ! {
    loop {
        let rc = sys::call(abi::FBCON_SUPERVISE, handle, 0, 0, 0);
        if (rc as i32) < 0 {
            sys::write(b"[init] fbcon start failed, retrying\n");
        } else {
            sys::write(b"[init] fbcon exited, restarting\n");
        }
        sys::sleep_ticks(100);
    }
}

extern "C" fn supervise_kbd(handle: u32) -> ! {
    loop {
        let rc = sys::call(abi::KBD_SUPERVISE, handle, 0, 0, 0);
        sys::write(if (rc as i32) < 0 {
            b"[init] keyboard start failed, retrying\n"
        } else {
            b"[init] keyboard exited, restarting\n"
        });
        sys::sleep_ticks(100);
    }
}

extern "C" fn supervise_ext4(handle: u32) -> ! {
    loop {
        let rc = sys::call(abi::EXT4_SUPERVISE, handle, 0, 0, 0);
        sys::write(if (rc as i32) < 0 {
            b"[init] ext4fs start failed, retrying\n"
        } else {
            b"[init] ext4fs exited, restarting\n"
        });
        sys::sleep_ticks(100);
    }
}

extern "C" fn supervise_sysd(handle: u32) -> ! {
    loop {
        let rc = sys::call(abi::SYSD_SUPERVISE, handle, 0, 0, 0);
        sys::write(if (rc as i32) < 0 {
            b"[init] sysd start failed, retrying\n"
        } else {
            b"[init] sysd exited, restarting\n"
        });
        sys::sleep_ticks(100);
    }
}

fn start_supervisor(entry: extern "C" fn(u32) -> !, supervisor: u32) {
    if supervisor != 0 {
        loop {
            if let Ok(mut stack) = vela_user::arena::Arena::new(8192) {
                let (tid, _) = sys::call6(
                    abi::THREAD_CREATE,
                    entry as *const () as u32,
                    stack.base() as u32,
                    stack.size() as u32,
                    supervisor,
                    1,
                    abi::AFFINITY_HP0 | abi::THREAD_STACK_GUARD,
                );
                if (tid as i32) > 0 {
                    break;
                }
                let _ = stack.release();
            }
            sys::write(b"[init] supervisor thread unavailable\n");
            sys::sleep_ticks(100);
        }
    }
}

fn main(vfs: u32, input: u32, spawn: u32, display: u32, supervisor: u32, keyboard: u32) -> ! {
    start_supervisor(supervise_fbcon, supervisor);
    start_supervisor(supervise_kbd, keyboard);
    start_supervisor(supervise_ext4, supervisor);
    start_supervisor(supervise_sysd, supervisor);
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
