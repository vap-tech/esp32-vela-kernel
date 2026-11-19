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
    /// Reserve a fixed-address, naturally aligned ELF segment in the SRAM pool.
    pub fn reserve(&mut self, owner: u32, base: usize, bytes: usize) -> Result<Region, u32> {
        let size = bytes.max(PAGE).checked_next_power_of_two().ok_or(22u32)?;
        if owner == 0 || bytes == 0 || base % size != 0 {
            return Err(22);
        }
        let first = base.checked_sub(self.base).ok_or(12u32)? / PAGE;
        let pages = size / PAGE;
        if first >= self.pages || pages > self.pages - first {
            return Err(12);
        }
        let mask = (u64::MAX >> (64 - pages)) << first;
        if self.occupied & mask != 0 {
            return Err(16);
        }
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.owner == 0)
            .ok_or(28u32)?;
        let region = Region { base, size };
        *entry = Allocation { owner, region };
        self.occupied |= mask;
        Ok(region)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_enforces_napot_alignment_and_owner_release() {
        // Start slightly unaligned to verify page rounding up to 0x4ff6_0000.
        let mut alloc = RegionAllocator::new(0x4ff5_f001, 0x4ff5_f001 + 32 * PAGE);
        assert_eq!(alloc.allocate(0, PAGE), Err(22));
        assert_eq!(alloc.allocate(1, 0), Err(22));

        let r1 = alloc.allocate(1, 100).unwrap();
        assert_eq!(r1.size, PAGE);
        assert_eq!(r1.base % r1.size, 0);
        assert!(alloc.owns(1, r1));
        assert!(!alloc.owns(2, r1));

        let r2 = alloc.allocate(2, 5000).unwrap();
        assert_eq!(r2.size, 2 * PAGE);
        assert_eq!(r2.base % r2.size, 0);
        assert_eq!(alloc.free(1, r2), Err(1));

        alloc.release_owner(1);
        assert!(!alloc.owns(1, r1));
        assert!(alloc.owns(2, r2));

        // Freed page can be allocated again.
        let r3 = alloc.allocate(3, PAGE).unwrap();
        assert_eq!(r3, r1);
    }

    #[test]
    fn reserve_detects_overlap_alignment_and_bounds() {
        let base = 0x4ff8_0000;
        let mut alloc = RegionAllocator::new(base, base + 8 * PAGE);
        let r = alloc.reserve(1, base + 2 * PAGE, 5000).unwrap();
        assert_eq!(
            r,
            Region {
                base: base + 2 * PAGE,
                size: 2 * PAGE,
            }
        );
        // Overlapping reservation is rejected with EBUSY (16).
        assert_eq!(alloc.reserve(2, base + 3 * PAGE, PAGE), Err(16));
        // Misaligned for 8 KiB NAPOT region -> EINVAL (22).
        assert_eq!(alloc.reserve(2, base + PAGE, 2 * PAGE), Err(22));
        // Below pool or beyond pool -> ENOMEM (12).
        assert_eq!(alloc.reserve(2, base - PAGE, PAGE), Err(12));
        assert_eq!(alloc.reserve(2, base + 8 * PAGE, PAGE), Err(12));
    }

    #[test]
    fn full_64_page_allocation_and_slot_limit() {
        let base = 0x5000_0000;
        let mut alloc = RegionAllocator::new(base, base + PAGES * PAGE);
        let whole = alloc.allocate(1, PAGES * PAGE).unwrap();
        assert_eq!(whole.base, base);
        assert_eq!(whole.size, PAGES * PAGE);
        assert_eq!(alloc.allocate(1, PAGE), Err(12));
        alloc.free(1, whole).unwrap();

        for _ in 0..ALLOCATIONS {
            alloc.allocate(1, PAGE).unwrap();
        }
        assert_eq!(alloc.allocate(1, PAGE), Err(28));
    }
}
