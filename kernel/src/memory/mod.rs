//! Fixed physical SRAM arena. All regions are NAPOT-compatible; no page tables.
//! Owner is a monotonic process ID, never a recyclable process slot.
use crate::task::Region;
pub const PAGE: usize = 4096;
const PAGES: usize = 64;
const ALLOCATIONS: usize = 24;
#[derive(Clone, Copy)]
struct Allocation {
    owner: u32,
    region: Region,
}
impl Allocation {
    const EMPTY: Self = Self {
        owner: 0,
        region: Region { base: 0, size: 0 },
    };
}
pub struct RegionAllocator {
    base: usize,
    pages: usize,
    occupied: u64,
    entries: [Allocation; ALLOCATIONS],
}
impl RegionAllocator {
    pub fn new(start: usize, end: usize) -> Self {
        let base = start.checked_add(PAGE - 1).unwrap() & !(PAGE - 1);
        let pages = (end.saturating_sub(base) / PAGE).min(PAGES);
        Self {
            base,
            pages,
            occupied: 0,
            entries: [Allocation::EMPTY; ALLOCATIONS],
        }
    }
    pub fn allocate(&mut self, owner: u32, requested: usize) -> Result<Region, u32> {
        if owner == 0 || requested == 0 {
            return Err(22);
        }
        let size = requested
            .max(PAGE)
            .checked_next_power_of_two()
            .ok_or(22u32)?;
        let count = size / PAGE;
        if count > self.pages {
            return Err(12);
        }
        let slot = self
            .entries
            .iter()
            .position(|e| e.owner == 0)
            .ok_or(28u32)?;
        for first in 0..=self.pages - count {
            let base = self.base + first * PAGE;
            if base % size != 0 {
                continue;
            }
            let mask = if count == 64 {
                u64::MAX
            } else {
                ((1u64 << count) - 1) << first
            };
            if self.occupied & mask != 0 {
                continue;
            }
            self.occupied |= mask;
            let region = Region { base, size };
            self.entries[slot] = Allocation { owner, region };
            return Ok(region);
        }
        Err(12)
    }
    pub fn owns(&self, owner: u32, region: Region) -> bool {
        self.entries
            .iter()
            .any(|e| e.owner == owner && e.region == region)
    }
    pub fn free(&mut self, owner: u32, region: Region) -> Result<(), u32> {
        let slot = self
            .entries
            .iter()
            .position(|e| e.owner == owner && e.region == region)
            .ok_or(1u32)?;
        let first = (region.base - self.base) / PAGE;
        let count = region.size / PAGE;
        let mask = if count == 64 {
            u64::MAX
        } else {
            ((1u64 << count) - 1) << first
        };
        self.occupied &= !mask;
        self.entries[slot] = Allocation::EMPTY;
        Ok(())
    }
    pub fn release_owner(&mut self, owner: u32) {
        for i in 0..ALLOCATIONS {
            let entry = self.entries[i];
            if entry.owner == owner {
                self.free(owner, entry.region).unwrap();
            }
        }
    }
}
