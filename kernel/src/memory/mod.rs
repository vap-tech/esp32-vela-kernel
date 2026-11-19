//! Fixed physical SRAM arena. All regions are NAPOT-compatible; no page tables.
//! Owner is a monotonic process ID, never a recyclable process slot.
use crate::{errno::Errno, id::Pid, task::Region};
pub const PAGE: usize = 4096;
const PAGES: usize = 64;
const ALLOCATIONS: usize = 24;
#[derive(Clone, Copy)]
struct Allocation {
    owner: Pid,
    region: Region,
}
impl Allocation {
    const EMPTY: Self = Self {
        owner: Pid::NONE,
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
    pub fn allocate(&mut self, owner: Pid, requested: usize) -> Result<Region, Errno> {
        if owner.is_none() || requested == 0 {
            return Err(Errno::Einval);
        }
        let size = requested
            .max(PAGE)
            .checked_next_power_of_two()
            .ok_or(Errno::Einval)?;
        let count = size / PAGE;
        if count > self.pages {
            return Err(Errno::Enomem);
        }
        let slot = self
            .entries
            .iter()
            .position(|e| e.owner.is_none())
            .ok_or(Errno::Enospc)?;
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
        Err(Errno::Enomem)
    }
    /// Reserve a fixed-address, naturally aligned ELF segment in the SRAM pool.
    pub fn reserve(&mut self, owner: Pid, base: usize, bytes: usize) -> Result<Region, Errno> {
        let size = bytes
            .max(PAGE)
            .checked_next_power_of_two()
            .ok_or(Errno::Einval)?;
        if owner.is_none() || bytes == 0 || base % size != 0 {
            return Err(Errno::Einval);
        }
        let first = base.checked_sub(self.base).ok_or(Errno::Enomem)? / PAGE;
        let pages = size / PAGE;
        if first >= self.pages || pages > self.pages - first {
            return Err(Errno::Enomem);
        }
        let mask = (u64::MAX >> (64 - pages)) << first;
        if self.occupied & mask != 0 {
            return Err(Errno::Ebusy);
        }
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.owner.is_none())
            .ok_or(Errno::Enospc)?;
        let region = Region { base, size };
        *entry = Allocation { owner, region };
        self.occupied |= mask;
        Ok(region)
    }
    pub fn owns(&self, owner: Pid, region: Region) -> bool {
        self.entries
            .iter()
            .any(|e| e.owner == owner && e.region == region)
    }
    pub fn free(&mut self, owner: Pid, region: Region) -> Result<(), Errno> {
        let slot = self
            .entries
            .iter()
            .position(|e| e.owner == owner && e.region == region)
            .ok_or(Errno::Eperm)?;
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
    pub fn release_owner(&mut self, owner: Pid) {
        for i in 0..ALLOCATIONS {
            let entry = self.entries[i];
            if entry.owner == owner {
                self.free(owner, entry.region).unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_enforces_napot_alignment_and_owner_release() {
        // Start slightly unaligned to verify page rounding up to 0x4ff6_0000.
        let mut alloc = RegionAllocator::new(0x4ff5_f001, 0x4ff5_f001 + 32 * PAGE);
        assert_eq!(alloc.allocate(Pid::NONE, PAGE), Err(Errno::Einval));
        assert_eq!(alloc.allocate(Pid(1), 0), Err(Errno::Einval));

        let r1 = alloc.allocate(Pid(1), 100).unwrap();
        assert_eq!(r1.size, PAGE);
        assert_eq!(r1.base % r1.size, 0);
        assert!(alloc.owns(Pid(1), r1));
        assert!(!alloc.owns(Pid(2), r1));

        let r2 = alloc.allocate(Pid(2), 5000).unwrap();
        assert_eq!(r2.size, 2 * PAGE);
        assert_eq!(r2.base % r2.size, 0);
        assert_eq!(alloc.free(Pid(1), r2), Err(Errno::Eperm));

        alloc.release_owner(Pid(1));
        assert!(!alloc.owns(Pid(1), r1));
        assert!(alloc.owns(Pid(2), r2));

        // Freed page can be allocated again.
        let r3 = alloc.allocate(Pid(3), PAGE).unwrap();
        assert_eq!(r3, r1);
    }

    #[test]
    fn reserve_detects_overlap_alignment_and_bounds() {
        let base = 0x4ff8_0000;
        let mut alloc = RegionAllocator::new(base, base + 8 * PAGE);
        let r = alloc.reserve(Pid(1), base + 2 * PAGE, 5000).unwrap();
        assert_eq!(
            r,
            Region {
                base: base + 2 * PAGE,
                size: 2 * PAGE,
            }
        );
        // Overlapping reservation is rejected with EBUSY.
        assert_eq!(
            alloc.reserve(Pid(2), base + 3 * PAGE, PAGE),
            Err(Errno::Ebusy)
        );
        // Misaligned for 8 KiB NAPOT region -> EINVAL.
        assert_eq!(
            alloc.reserve(Pid(2), base + PAGE, 2 * PAGE),
            Err(Errno::Einval)
        );
        // Below pool or beyond pool -> ENOMEM.
        assert_eq!(alloc.reserve(Pid(2), base - PAGE, PAGE), Err(Errno::Enomem));
        assert_eq!(
            alloc.reserve(Pid(2), base + 8 * PAGE, PAGE),
            Err(Errno::Enomem)
        );
    }

    #[test]
    fn full_64_page_allocation_and_slot_limit() {
        let base = 0x5000_0000;
        let mut alloc = RegionAllocator::new(base, base + PAGES * PAGE);
        let whole = alloc.allocate(Pid(1), PAGES * PAGE).unwrap();
        assert_eq!(whole.base, base);
        assert_eq!(whole.size, PAGES * PAGE);
        assert_eq!(alloc.allocate(Pid(1), PAGE), Err(Errno::Enomem));
        alloc.free(Pid(1), whole).unwrap();

        for _ in 0..ALLOCATIONS {
            alloc.allocate(Pid(1), PAGE).unwrap();
        }
        assert_eq!(alloc.allocate(Pid(1), PAGE), Err(Errno::Enospc));
    }
}
