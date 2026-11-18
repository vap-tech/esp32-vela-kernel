//! Process-owned address spaces. Slot indices never cross the user ABI.
use crate::{capability::Table, task::Region};
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
    pub fn insert(&mut self, region: Region, permissions: u8) -> Result<(), u32> {
        if region.size < 128
            || !region.size.is_power_of_two()
            || region.base % region.size != 0
            || region.base.checked_add(region.size).is_none()
            || permissions & !7 != 0
            || permissions & WRITE != 0 && permissions & READ == 0
        {
            return Err(22);
        }
        if permissions == 0
            || permissions & (WRITE | EXECUTE) == WRITE | EXECUTE
            || self.maps.iter().any(|m| {
                m.region.size != 0
                    && region.base < m.region.base + m.region.size
                    && m.region.base < region.base + region.size
            })
        {
            return Err(22);
        }
        let index = self
            .maps
            .iter()
            .position(|m| m.region.size == 0)
            .ok_or(28u32)?;
        self.maps[index] = Mapping {
            region,
            permissions,
        };
        self.revision = self.revision.checked_add(1).ok_or(75u32)?;
        Ok(())
    }
    pub fn remove(&mut self, region: Region) -> Result<(), u32> {
        let index = self
            .maps
            .iter()
            .position(|m| m.region == region && region.size != 0)
            .ok_or(22u32)?;
        self.maps[index] = Mapping::default();
        self.revision = self.revision.checked_add(1).ok_or(75u32)?;
        Ok(())
    }
    /// Keep the mapping slot reserved while device ownership denies U-mode access.
    pub fn dma_access(&mut self, region: Region, allow: bool) -> Result<(), u32> {
        let revision = self.revision.checked_add(1).ok_or(75u32)?;
        let m = self
            .maps
            .iter_mut()
            .find(|m| m.region == region && region.size != 0)
            .ok_or(22u32)?;
        let rw = READ | WRITE;
        if m.permissions != if allow { 0 } else { rw } {
            return Err(22);
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
    pub fn create(&mut self, memory: AddressSpace) -> Result<usize, u32> {
        let index = self
            .slots
            .iter()
            .position(|p| p.state == Lifecycle::Free)
            .ok_or(28u32)?;
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or(75u32)?;
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
