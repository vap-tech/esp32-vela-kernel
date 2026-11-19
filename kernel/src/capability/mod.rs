//! Process-local, generational capabilities. Grants are deliberately one-level:
//! recipients cannot forward GRANT/CONTROL; owner revocation is object-wide.
use crate::task::Region;
pub const READ: u16 = 1;
pub const WRITE: u16 = 2;
pub const EXECUTE: u16 = 4;
pub const SEND: u16 = 8;
pub const RECEIVE: u16 = 16;
pub const GRANT: u16 = 32;
pub const WAIT: u16 = 64;
pub const ACK: u16 = 128;
pub const CONTROL: u16 = 256;
pub const EBADF: u32 = (-9i32) as u32;
pub const EPERM: u32 = (-1i32) as u32;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    Endpoint { index: usize, generation: u32 },
    Memory { owner: u32, region: Region },
    Event { index: usize, generation: u32 },
}
#[derive(Clone, Copy)]
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
    pub fn install(&mut self, cap: Capability) -> Result<u32, u32> {
        let (i, s) = self
            .slots
            .iter_mut()
            .enumerate()
            .find(|(_, s)| s.cap.is_none() && s.generation < 0x7f_ffff)
            .ok_or((-28i32) as u32)?;
        s.generation += 1;
        s.cap = Some(cap);
        Ok((s.generation << 8) | (i as u32 + 1))
    }
    fn slot(&self, handle: u32) -> Result<usize, u32> {
        let i = (handle & 255).wrapping_sub(1) as usize;
        let s = self.slots.get(i).ok_or(EBADF)?;
        if s.generation != handle >> 8 || s.cap.is_none() {
            return Err(EBADF);
        }
        Ok(i)
    }
    pub fn get(&self, handle: u32, rights: u16) -> Result<Capability, u32> {
        let cap = self.slots[self.slot(handle)?].cap.ok_or(EBADF)?;
        if cap.rights & rights != rights {
            return Err(EPERM);
        }
        Ok(cap)
    }
    pub fn close(&mut self, handle: u32) -> Result<Capability, u32> {
        let i = self.slot(handle)?;
        self.slots[i].cap.take().ok_or(EBADF)
    }
    pub fn revoke(&mut self, object: Object) {
        for s in &mut self.slots {
            if s.cap.is_some_and(|c| c.object == object) {
                s.cap = None;
            }
        }
    }
    pub fn find(&self, object: Object, rights: u16) -> Option<u32> {
        self.slots
            .iter()
            .enumerate()
            .find(|(_, s)| {
                s.cap
                    .is_some_and(|c| c.object == object && c.rights & rights == rights)
            })
            .map(|(i, s)| (s.generation << 8) | (i as u32 + 1))
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
        assert_eq!(table.get(handle, CONTROL).err(), Some(EPERM));
        assert_eq!(table.find(ep, RECEIVE), Some(handle));
        assert_eq!(table.find(ep, CONTROL), None);

        table.close(handle).unwrap();
        assert_eq!(table.get(handle, 0).err(), Some(EBADF));

        let reused = table
            .install(Capability {
                object: ep,
                rights: SEND,
            })
            .unwrap();
        assert_ne!(reused, handle);
        assert_eq!(table.get(handle, 0).err(), Some(EBADF));
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
        assert_eq!(table.get(h_a1, 0).err(), Some(EBADF));
        assert_eq!(table.get(h_a2, 0).err(), Some(EBADF));
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
            Some((-28i32) as u32)
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
            Some((-28i32) as u32)
        );
    }
}
