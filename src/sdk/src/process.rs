//! Process creation requires an explicit spawn authority. Normal shell-launched
//! applications do not inherit it. Children inherit only the supplied VFS handle.
use crate::{Error, Result, fs::FileSystem};
use vela_abi::syscall as abi;
use vela_user::{Buffer, file, sys};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pid(pub u32);

pub struct Spawner {
    handle: u32,
}

impl Spawner {
    /// Requires EXECUTE authority. Forwarding inherited VFS also requires the
    /// spawn policy's CONTROL permission; the kernel validates both permissions.
    pub const fn from_handle(handle: u32) -> Self {
        Self { handle }
    }

    /// Load an ELF and wait for the child to exit. Returns its PID, not exit status.
    /// Child stdin is absent and stdout goes to the console.
    pub fn run(&self, files: FileSystem, path: &[u8], argument: &[u8]) -> Result<Pid> {
        if self.handle == 0 {
            return Err(Error::NoCapability);
        }
        if argument.len() > abi::SPAWN_ARG_MAX {
            return Err(Error::InvalidArgument);
        }
        crate::fs::validate_path(path)?;
        let vfs = files.handle()?;
        let (mut arena, length) =
            file::fs_load_arena(vfs, path, abi::SPAWN_IMAGE_MAX, &mut Buffer::new())
                .map_err(|_| Error::Io)?;
        let caps = abi::SpawnCaps {
            count: 1,
            caps: [
                abi::SpawnCap {
                    handle: vfs,
                    rights: abi::CAP_SEND,
                },
                abi::SpawnCap::EMPTY,
                abi::SpawnCap::EMPTY,
                abi::SpawnCap::EMPTY,
            ],
        };
        let (pid, _) = sys::call6(
            abi::SPAWN_IMAGE_WAIT,
            self.handle,
            arena.base() as u32,
            length as u32,
            argument.as_ptr() as u32,
            argument.len() as u32,
            &caps as *const _ as u32,
        );
        let _ = arena.release();
        if (pid as i32) < 0 {
            Err(Error::Kernel(pid))
        } else {
            Ok(Pid(pid))
        }
    }
}

pub fn sleep_ticks(ticks: u32) {
    sys::sleep_ticks(ticks);
}

/// Exit closes piped stdout so downstream readers receive EOF.
pub fn exit() -> ! {
    vela_user::io::close_stdout();
    sys::process_exit()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absent_spawn_authority_is_reported_before_loading_an_image() {
        assert_eq!(
            Spawner::from_handle(0).run(FileSystem::from_handle(1), b"/sd/bin/upper.elf", b""),
            Err(Error::NoCapability)
        );
    }
}
