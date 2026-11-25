//! Process-local, generational capabilities. Grants are deliberately one-level:
//! recipients cannot forward GRANT/CONTROL; owner revocation is object-wide.
use crate::{
    errno::Errno,
    id::{Handle, Pid},
    task::Region,
};
pub const READ: u16 = 1;
pub const WRITE: u16 = 2;
pub const EXECUTE: u16 = 4;
pub const SEND: u16 = 8;
pub const RECEIVE: u16 = 16;
pub const GRANT: u16 = 32;
pub const WAIT: u16 = 64;
pub const ACK: u16 = 128;
pub const CONTROL: u16 = 256;
/// Boot policy may allow a send endpoint to be installed in a new child.
/// The child receives SEND alone and cannot pass it to another process.
pub const PASS_ON_SPAWN: u16 = 512;
/// Unforgeable administrative privilege on a service endpoint (e.g. `sysd` mount/unmount).
pub const ADMIN: u16 = 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authority {
    ProcessSpawn,
    FbconSupervisor,
    /// Boot-only authority; the handle names the terminal endpoint owned by init.
    KeyboardSupervisor(Handle),
    Display,
    Sentinel,
    IrqControl,
    /// Read access to the ring of console output (`CONSOLE_LOG`).
    ConsoleLog,
    /// One board I2C bus (`vela_abi::I2C_BUS_*`); policy per bus lives in the BSP.
    I2c(u8),
    SdSystemPartition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    Endpoint { index: usize, generation: u32 },
    Memory { owner: Pid, region: Region },
    Event { index: usize, generation: u32 },
    DmaChannel { index: usize, generation: u32 },
    Authority(Authority),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability {
    pub object: Object,
    pub rights: u16,
}
#[derive(Clone, Copy)]
struct Slot {
    cap: Option<Capability>,
    generation: u32,
}
impl Slot {
    const EMPTY: Self = Self {
        cap: None,
        generation: 0,
    };
}
#[derive(Clone, Copy)]
pub struct Table {
    slots: [Slot; 16],
}
impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}
impl Table {
    pub const fn new() -> Self {
        Self {
            slots: [Slot::EMPTY; 16],
        }
    }
    pub fn install(&mut self, cap: Capability) -> Result<Handle, Errno> {
        let (i, s) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, s)| s.cap.is_none() && s.generation < 0x7f_ffff)
            .ok_or(Errno::Enospc)?;
        s.generation += 1;
        s.cap = Some(cap);
        Ok(Handle((s.generation << 8) | (i as u32 + 1)))
    }
    fn slot(&self, handle: Handle) -> Result<usize, Errno> {
        let raw = handle.raw();
        let i = (raw & 255).wrapping_sub(1) as usize;
        let s = self.slots.get(i).ok_or(Errno::Ebadf)?;
        if s.generation != raw >> 8 || s.cap.is_none() {
            return Err(Errno::Ebadf);
        }
        Ok(i)
    }
    pub fn get(&self, handle: Handle, rights: u16) -> Result<Capability, Errno> {
        let cap = self.slots[self.slot(handle)?].cap.ok_or(Errno::Ebadf)?;
        if cap.rights & rights != rights {
            return Err(Errno::Eperm);
        }
        Ok(cap)
    }
    pub fn close(&mut self, handle: Handle) -> Result<Capability, Errno> {
        let i = self.slot(handle)?;
        self.slots[i].cap.take().ok_or(Errno::Ebadf)
    }
    pub fn revoke(&mut self, object: Object) {
        for s in &mut self.slots {
            if s.cap.is_some_and(|c| c.object == object) {
                s.cap = None;
            }
        }
    }
    pub fn find(&self, object: Object, rights: u16) -> Option<Handle> {
        self.slots
            .iter()
            .enumerate()
            .find(|(_, s)| {
                s.cap
                    .is_some_and(|c| c.object == object && c.rights & rights == rights)
            })
            .map(|(i, s)| Handle((s.generation << 8) | (i as u32 + 1)))
    }
    pub fn clear(&mut self) {
        for s in &mut self.slots {
            s.cap = None;
        }
    }
    pub fn objects(&self) -> [Option<Capability>; 16] {
        core::array::from_fn(|i| self.slots[i].cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generational_handles_reject_stale_reuse_and_enforce_rights() {
        let mut table = Table::new();
        let ep = Object::Endpoint {
            index: 1,
            generation: 1,
        };
        let handle = table
            .install(Capability {
                object: ep,
                rights: SEND | RECEIVE,
            })
            .unwrap();
        assert_eq!(table.get(handle, SEND).unwrap().object, ep);
        assert_eq!(table.get(handle, CONTROL).err(), Some(Errno::Eperm));
        assert_eq!(table.find(ep, RECEIVE), Some(handle));
        assert_eq!(table.find(ep, CONTROL), None);

        table.close(handle).unwrap();
        assert_eq!(table.get(handle, 0).err(), Some(Errno::Ebadf));

        let reused = table
            .install(Capability {
                object: ep,
                rights: SEND,
            })
            .unwrap();
        assert_ne!(reused, handle);
        assert_eq!(table.get(handle, 0).err(), Some(Errno::Ebadf));
        assert_eq!(table.get(reused, SEND).unwrap().object, ep);
    }

    #[test]
    fn revoke_clears_matching_object_only_and_enforces_capacity_and_generation_limit() {
        let mut table = Table::new();
        let ep_a = Object::Endpoint {
            index: 0,
            generation: 1,
        };
        let ep_b = Object::Endpoint {
            index: 1,
            generation: 1,
        };
        let h_a1 = table
            .install(Capability {
                object: ep_a,
                rights: SEND,
            })
            .unwrap();
        let h_b = table
            .install(Capability {
                object: ep_b,
                rights: SEND,
            })
            .unwrap();
        let h_a2 = table
            .install(Capability {
                object: ep_a,
                rights: RECEIVE,
            })
            .unwrap();

        table.revoke(ep_a);
        assert_eq!(table.get(h_a1, 0).err(), Some(Errno::Ebadf));
        assert_eq!(table.get(h_a2, 0).err(), Some(Errno::Ebadf));
        assert_eq!(table.get(h_b, SEND).unwrap().object, ep_b);

        for _ in 0..15 {
            table
                .install(Capability {
                    object: ep_b,
                    rights: SEND,
                })
                .unwrap();
        }
        assert_eq!(
            table
                .install(Capability {
                    object: ep_b,
                    rights: SEND,
                })
                .err(),
            Some(Errno::Enospc)
        );

        let mut exhausted = Table::new();
        for s in &mut exhausted.slots {
            s.generation = 0x7f_ffff;
        }
        assert_eq!(
            exhausted
                .install(Capability {
                    object: ep_a,
                    rights: SEND,
                })
                .err(),
            Some(Errno::Enospc)
        );
    }
}
