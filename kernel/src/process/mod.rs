//! Process-owned address spaces. Slot indices never cross the user ABI.
use crate::{capability::Table, errno::Errno, task::Region};
pub const PROCESSES: usize = 8;
pub const REGIONS: usize = 6;
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Free,
    Alive,
    Dying,
}
#[derive(Clone, Copy, Default)]
pub struct Mapping {
    pub region: Region,
    pub permissions: u8,
}
pub const READ: u8 = 1;
pub const WRITE: u8 = 2;
pub const EXECUTE: u8 = 4;
#[derive(Clone, Copy)]
pub struct AddressSpace {
    pub maps: [Mapping; REGIONS],
    pub revision: u32,
}
impl AddressSpace {
    pub const EMPTY: Self = Self {
        maps: [Mapping {
            region: Region { base: 0, size: 0 },
            permissions: 0,
        }; REGIONS],
        revision: 0,
    };
    pub fn insert(&mut self, region: Region, permissions: u8) -> Result<(), Errno> {
        if region.size < 128
            || !region.size.is_power_of_two()
            || region.base % region.size != 0
            || region.base.checked_add(region.size).is_none()
            || permissions & !7 != 0
            || permissions & WRITE != 0 && permissions & READ == 0
        {
            return Err(Errno::Einval);
        }
        if permissions == 0
            || permissions & (WRITE | EXECUTE) == WRITE | EXECUTE
            || self.maps.iter().any(|m| {
                m.region.size != 0
                    && region.base < m.region.base + m.region.size
                    && m.region.base < region.base + region.size
            })
        {
            return Err(Errno::Einval);
        }
        let index = self
            .maps
            .iter()
            .position(|m| m.region.size == 0)
            .ok_or(Errno::Enospc)?;
        self.maps[index] = Mapping {
            region,
            permissions,
        };
        self.revision = self.revision.checked_add(1).ok_or(Errno::Eoverflow)?;
        Ok(())
    }
    pub fn remove(&mut self, region: Region) -> Result<(), Errno> {
        let index = self
            .maps
            .iter()
            .position(|m| m.region == region && region.size != 0)
            .ok_or(Errno::Einval)?;
        self.maps[index] = Mapping::default();
        self.revision = self.revision.checked_add(1).ok_or(Errno::Eoverflow)?;
        Ok(())
    }
    /// Keep the mapping slot reserved while device ownership denies U-mode access.
    pub fn dma_access(&mut self, region: Region, allow: bool) -> Result<(), Errno> {
        let revision = self.revision.checked_add(1).ok_or(Errno::Eoverflow)?;
        let m = self
            .maps
            .iter_mut()
            .find(|m| m.region == region && region.size != 0)
            .ok_or(Errno::Einval)?;
        let rw = READ | WRITE;
        if m.permissions != if allow { 0 } else { rw } {
            return Err(Errno::Einval);
        }
        m.permissions = if allow { rw } else { 0 };
        self.revision = revision;
        Ok(())
    }
    pub fn permits(&self, address: usize, length: usize, rights: u8) -> bool {
        self.maps.iter().any(|m| {
            m.region.size != 0
                && m.permissions & rights == rights
                && m.region.contains(address, length)
        })
    }
}
#[derive(Clone, Copy)]
pub struct Process {
    pub id: u32,
    pub state: Lifecycle,
    pub memory: AddressSpace,
    pub capabilities: Table,
    pub priority_ceiling: u8,
}
impl Process {
    pub const EMPTY: Self = Self {
        id: 0,
        state: Lifecycle::Free,
        memory: AddressSpace::EMPTY,
        capabilities: Table::new(),
        priority_ceiling: 1,
    };
}
pub struct Processes {
    pub slots: [Process; PROCESSES],
    next_id: u32,
}
impl Default for Processes {
    fn default() -> Self {
        Self::new()
    }
}
impl Processes {
    pub const fn new() -> Self {
        Self {
            slots: [Process::EMPTY; PROCESSES],
            next_id: 1,
        }
    }
    pub fn create(&mut self, memory: AddressSpace) -> Result<usize, Errno> {
        let index = self
            .slots
            .iter()
            .position(|p| p.state == Lifecycle::Free)
            .ok_or(Errno::Enospc)?;
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or(Errno::Eoverflow)?;
        self.slots[index] = Process {
            id,
            state: Lifecycle::Alive,
            memory,
            capabilities: Table::new(),
            priority_ceiling: 1,
        };
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_space_enforces_wx_alignment_overlap_and_bounds() {
        let mut space = AddressSpace::EMPTY;
        let code = Region {
            base: 0x4ff4_0000,
            size: 8192,
        };
        assert_eq!(
            space.insert(code, READ | WRITE | EXECUTE),
            Err(Errno::Einval)
        );
        assert_eq!(space.insert(code, WRITE), Err(Errno::Einval));
        assert_eq!(space.insert(code, 0), Err(Errno::Einval));
        assert_eq!(
            space.insert(
                Region {
                    base: 0x4ff4_1000,
                    size: 8192,
                },
                READ | EXECUTE,
            ),
            Err(Errno::Einval)
        );
        assert_eq!(
            space.insert(
                Region {
                    base: 0x4ff4_0000,
                    size: 64,
                },
                READ,
            ),
            Err(Errno::Einval)
        );

        space.insert(code, READ | EXECUTE).unwrap();
        assert_eq!(space.revision, 1);
        assert!(space.permits(code.base, code.size, READ | EXECUTE));
        assert!(!space.permits(code.base, code.size, WRITE));
        assert!(!space.permits(code.base + code.size - 1, 2, READ));
        assert!(!space.permits(usize::MAX, 1, READ));

        // Overlapping mapping rejected.
        assert_eq!(
            space.insert(
                Region {
                    base: 0x4ff4_1000,
                    size: 4096,
                },
                READ | WRITE,
            ),
            Err(Errno::Einval)
        );

        space.remove(code).unwrap();
        assert_eq!(space.revision, 2);
        assert!(!space.permits(code.base, 1, READ));
    }

    #[test]
    fn address_space_and_process_table_enforce_limits_and_monotonic_ids() {
        let mut space = AddressSpace::EMPTY;
        for i in 0..REGIONS {
            space
                .insert(
                    Region {
                        base: 0x4ff4_0000 + i * 4096,
                        size: 4096,
                    },
                    READ | WRITE,
                )
                .unwrap();
        }
        assert_eq!(
            space.insert(
                Region {
                    base: 0x4ff4_0000 + REGIONS * 4096,
                    size: 4096,
                },
                READ | WRITE,
            ),
            Err(Errno::Enospc)
        );

        let mut procs = Processes::new();
        for i in 0..PROCESSES {
            let slot = procs.create(AddressSpace::EMPTY).unwrap();
            assert_eq!(slot, i);
            assert_eq!(procs.slots[slot].id, i as u32 + 1);
        }
        assert_eq!(procs.create(AddressSpace::EMPTY), Err(Errno::Enospc));

        procs.slots[0] = Process::EMPTY;
        let reused = procs.create(AddressSpace::EMPTY).unwrap();
        assert_eq!(reused, 0);
        assert_eq!(procs.slots[reused].id, PROCESSES as u32 + 1);
    }
}
