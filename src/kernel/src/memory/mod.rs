//! Fixed physical memory arenas (internal SRAM and external PSRAM).
//! All regions are NAPOT-compatible; no page tables.
//! Owner is a monotonic process ID (or `Pid::KERNEL` for trusted kernel subsystems),
//! never a recyclable process slot. Regions actively bound to a DMA channel are
//! marked `in_flight_channel` and cannot be freed or reused until DMA completes.
use crate::{errno::Errno, id::Pid, task::Region};

pub const PAGE: usize = 4096;
pub const PSRAM_MMU_PAGE: usize = 64 * 1024;
const PAGES: usize = 64;
const WORDS: usize = 8;
pub const MAX_POOL_PAGES: usize = WORDS * 64;
const ALLOCATIONS: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryClass {
    Sram,
    Psram,
}

impl MemoryClass {
    pub const fn from_abi(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Sram),
            1 => Some(Self::Psram),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct Allocation {
    owner: Pid,
    region: Region,
    in_flight_channel: Option<u8>,
}

impl Allocation {
    const EMPTY: Self = Self {
        owner: Pid::NONE,
        region: Region { base: 0, size: 0 },
        in_flight_channel: None,
    };
}

pub struct RegionPool {
    base: usize,
    page_size: usize,
    pages: usize,
    occupied: [u64; WORDS],
    entries: [Allocation; ALLOCATIONS],
}

impl RegionPool {
    pub const fn empty(page_size: usize) -> Self {
        Self {
            base: 0,
            page_size,
            pages: 0,
            occupied: [0; WORDS],
            entries: [Allocation::EMPTY; ALLOCATIONS],
        }
    }

    pub fn new(start: usize, end: usize, page_size: usize) -> Self {
        assert!(page_size.is_power_of_two() && page_size >= 128);
        let base = start.checked_add(page_size - 1).unwrap() & !(page_size - 1);
        let pages = (end.saturating_sub(base) / page_size).min(MAX_POOL_PAGES);
        Self {
            base,
            page_size,
            pages,
            occupied: [0; WORDS],
            entries: [Allocation::EMPTY; ALLOCATIONS],
        }
    }

    pub const fn is_configured(&self) -> bool {
        self.pages != 0
    }

    pub const fn base(&self) -> usize {
        self.base
    }

    pub const fn page_size(&self) -> usize {
        self.page_size
    }

    pub const fn pages(&self) -> usize {
        self.pages
    }

    pub fn contains(&self, region: Region) -> bool {
        if self.pages == 0 || region.size == 0 || region.base < self.base {
            return false;
        }
        let Some(end) = region.base.checked_add(region.size) else {
            return false;
        };
        let Some(pool_end) = self.base.checked_add(self.pages * self.page_size) else {
            return false;
        };
        end <= pool_end
    }

    fn range_free(&self, first: usize, count: usize) -> bool {
        for idx in first..first + count {
            if (self.occupied[idx / 64] & (1u64 << (idx % 64))) != 0 {
                return false;
            }
        }
        true
    }

    fn set_range(&mut self, first: usize, count: usize, used: bool) {
        for idx in first..first + count {
            let bit = 1u64 << (idx % 64);
            if used {
                self.occupied[idx / 64] |= bit;
            } else {
                self.occupied[idx / 64] &= !bit;
            }
        }
    }

    pub fn allocate(&mut self, owner: Pid, requested: usize) -> Result<Region, Errno> {
        if owner.is_none() || requested == 0 {
            return Err(Errno::Einval);
        }
        if self.pages == 0 {
            return Err(Errno::Enomem);
        }
        let size = requested
            .max(self.page_size)
            .checked_next_power_of_two()
            .ok_or(Errno::Einval)?;
        let count = size / self.page_size;
        if count > self.pages {
            return Err(Errno::Enomem);
        }
        let slot = self
            .entries
            .iter()
            .position(|e| e.owner.is_none())
            .ok_or(Errno::Enospc)?;
        for first in 0..=self.pages - count {
            let base = self.base + first * self.page_size;
            if base % size != 0 || !self.range_free(first, count) {
                continue;
            }
            self.set_range(first, count, true);
            let region = Region { base, size };
            self.entries[slot] = Allocation {
                owner,
                region,
                in_flight_channel: None,
            };
            return Ok(region);
        }
        Err(Errno::Enomem)
    }

    pub fn reserve(&mut self, owner: Pid, base: usize, bytes: usize) -> Result<Region, Errno> {
        let size = bytes
            .max(self.page_size)
            .checked_next_power_of_two()
            .ok_or(Errno::Einval)?;
        if owner.is_none() || bytes == 0 || base % size != 0 {
            return Err(Errno::Einval);
        }
        if self.pages == 0 {
            return Err(Errno::Enomem);
        }
        let offset = base.checked_sub(self.base).ok_or(Errno::Enomem)?;
        if offset % self.page_size != 0 {
            return Err(Errno::Einval);
        }
        let first = offset / self.page_size;
        let pages = size / self.page_size;
        if first >= self.pages || pages > self.pages - first {
            return Err(Errno::Enomem);
        }
        if !self.range_free(first, pages) {
            return Err(Errno::Ebusy);
        }
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.owner.is_none())
            .ok_or(Errno::Enospc)?;
        let region = Region { base, size };
        *entry = Allocation {
            owner,
            region,
            in_flight_channel: None,
        };
        self.set_range(first, pages, true);
        Ok(region)
    }

    pub fn owns(&self, owner: Pid, region: Region) -> bool {
        self.entries
            .iter()
            .any(|e| e.owner == owner && e.region == region)
    }

    /// The allocation owned by `owner` that starts at `base`.
    pub fn find_owned(&self, owner: Pid, base: usize) -> Option<Region> {
        self.entries
            .iter()
            .find(|e| e.owner == owner && e.region.base == base)
            .map(|e| e.region)
    }

    pub fn is_in_flight(&self, region: Region) -> bool {
        self.entries
            .iter()
            .any(|e| !e.owner.is_none() && e.region == region && e.in_flight_channel.is_some())
    }

    pub fn set_in_flight(
        &mut self,
        owner: Pid,
        region: Region,
        channel: Option<u8>,
    ) -> Result<(), Errno> {
        let entry = self
            .entries
            .iter_mut()
            .find(|e| e.owner == owner && e.region == region)
            .ok_or(Errno::Eperm)?;
        match (entry.in_flight_channel, channel) {
            (None, Some(ch)) => {
                entry.in_flight_channel = Some(ch);
                Ok(())
            }
            (Some(_), None) => {
                entry.in_flight_channel = None;
                Ok(())
            }
            (Some(_), Some(_)) => Err(Errno::Ebusy),
            (None, None) => Err(Errno::Einval),
        }
    }

    pub fn free(&mut self, owner: Pid, region: Region) -> Result<(), Errno> {
        let slot = self
            .entries
            .iter()
            .position(|e| e.owner == owner && e.region == region)
            .ok_or(Errno::Eperm)?;
        if self.entries[slot].in_flight_channel.is_some() {
            return Err(Errno::Ebusy);
        }
        let first = (region.base - self.base) / self.page_size;
        let count = region.size / self.page_size;
        self.set_range(first, count, false);
        self.entries[slot] = Allocation::EMPTY;
        Ok(())
    }

    pub fn release_owner(&mut self, owner: Pid) {
        self.release_owner_with(owner, |_| {});
    }

    /// Release all regions owned by `owner` that are not currently `in_flight_channel`.
    /// Any region with active DMA stays reserved until [`Self::finish_in_flight`] completes.
    pub fn release_owner_with(&mut self, owner: Pid, mut on_free: impl FnMut(Region)) {
        for i in 0..ALLOCATIONS {
            let entry = self.entries[i];
            if entry.owner == owner && entry.in_flight_channel.is_none() {
                self.free(owner, entry.region).unwrap();
                on_free(entry.region);
            }
        }
    }

    /// Complete an in-flight DMA hold on `region` for `channel`.
    /// If `owner_still_alive` is false (process exited while DMA was active) and `success`
    /// is true, frees `region` immediately and returns `true` so the caller can unmap MMU.
    /// If `success` is false, leaves the region quarantined (`in_flight_channel` retained).
    pub fn finish_in_flight(
        &mut self,
        region: Region,
        channel: u8,
        owner_still_alive: bool,
        success: bool,
    ) -> Result<bool, Errno> {
        let slot = self
            .entries
            .iter()
            .position(|e| {
                !e.owner.is_none() && e.region == region && e.in_flight_channel == Some(channel)
            })
            .ok_or(Errno::Einval)?;
        if !success {
            return Ok(false);
        }
        self.entries[slot].in_flight_channel = None;
        if !owner_still_alive {
            let owner = self.entries[slot].owner;
            self.free(owner, region).unwrap();
            return Ok(true);
        }
        Ok(false)
    }
}

pub struct RegionAllocator {
    sram: RegionPool,
    psram: RegionPool,
}

impl RegionAllocator {
    pub fn new(start: usize, end: usize) -> Self {
        let base = start.checked_add(PAGE - 1).unwrap() & !(PAGE - 1);
        let sram_end = base.saturating_add(PAGES * PAGE).min(end);
        Self {
            sram: RegionPool::new(start, sram_end, PAGE),
            psram: RegionPool::empty(PSRAM_MMU_PAGE),
        }
    }

    /// Configure the managed external 32 MiB PSRAM pool (`page_size` must be a power of two >= 4 KiB).
    pub fn configure_psram(&mut self, start: usize, end: usize, page_size: usize) {
        self.psram = RegionPool::new(start, end, page_size);
    }

    pub const fn psram_configured(&self) -> bool {
        self.psram.is_configured()
    }

    pub const fn psram_pool(&self) -> &RegionPool {
        &self.psram
    }

    pub fn is_psram(&self, region: Region) -> bool {
        self.psram.contains(region)
    }

    pub fn allocate(&mut self, owner: Pid, requested: usize) -> Result<Region, Errno> {
        self.sram.allocate(owner, requested)
    }

    pub fn allocate_in(
        &mut self,
        class: MemoryClass,
        owner: Pid,
        requested: usize,
    ) -> Result<Region, Errno> {
        match class {
            MemoryClass::Sram => self.sram.allocate(owner, requested),
            MemoryClass::Psram => self.psram.allocate(owner, requested),
        }
    }

    /// Reserve a fixed-address, naturally aligned ELF segment in the SRAM pool.
    pub fn reserve(&mut self, owner: Pid, base: usize, bytes: usize) -> Result<Region, Errno> {
        self.sram.reserve(owner, base, bytes)
    }

    pub fn owns(&self, owner: Pid, region: Region) -> bool {
        self.sram.owns(owner, region) || self.psram.owns(owner, region)
    }

    /// The region `owner` allocated at exactly `base`, in either pool.
    pub fn find_owned(&self, owner: Pid, base: usize) -> Option<Region> {
        self.sram
            .find_owned(owner, base)
            .or_else(|| self.psram.find_owned(owner, base))
    }

    pub fn owns_in(&self, class: MemoryClass, owner: Pid, region: Region) -> bool {
        match class {
            MemoryClass::Sram => self.sram.owns(owner, region),
            MemoryClass::Psram => self.psram.owns(owner, region),
        }
    }

    pub fn is_in_flight(&self, region: Region) -> bool {
        self.sram.is_in_flight(region) || self.psram.is_in_flight(region)
    }

    pub fn set_in_flight(
        &mut self,
        owner: Pid,
        region: Region,
        channel: Option<u8>,
    ) -> Result<MemoryClass, Errno> {
        if self.sram.owns(owner, region) {
            self.sram.set_in_flight(owner, region, channel)?;
            Ok(MemoryClass::Sram)
        } else if self.psram.owns(owner, region) {
            self.psram.set_in_flight(owner, region, channel)?;
            Ok(MemoryClass::Psram)
        } else {
            Err(Errno::Eperm)
        }
    }

    pub fn finish_in_flight(
        &mut self,
        region: Region,
        channel: u8,
        owner_still_alive: bool,
        success: bool,
    ) -> Result<(MemoryClass, bool), Errno> {
        if self.sram.contains(region) {
            let freed = self
                .sram
                .finish_in_flight(region, channel, owner_still_alive, success)?;
            Ok((MemoryClass::Sram, freed))
        } else if self.psram.contains(region) {
            let freed = self
                .psram
                .finish_in_flight(region, channel, owner_still_alive, success)?;
            Ok((MemoryClass::Psram, freed))
        } else {
            Err(Errno::Einval)
        }
    }

    pub fn free(&mut self, owner: Pid, region: Region) -> Result<MemoryClass, Errno> {
        if self.sram.owns(owner, region) {
            self.sram.free(owner, region)?;
            Ok(MemoryClass::Sram)
        } else if self.psram.owns(owner, region) {
            self.psram.free(owner, region)?;
            Ok(MemoryClass::Psram)
        } else {
            Err(Errno::Eperm)
        }
    }

    pub fn release_owner(&mut self, owner: Pid) {
        self.sram.release_owner(owner);
        self.psram.release_owner(owner);
    }

    pub fn release_psram_owner_with(&mut self, owner: Pid, on_free: impl FnMut(Region)) {
        self.psram.release_owner_with(owner, on_free);
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
    fn find_owned_requires_matching_owner_and_exact_base() {
        let mut alloc = RegionAllocator::new(0x4ff5_f001, 0x4ff5_f001 + 32 * PAGE);
        let r = alloc.allocate(Pid(1), PAGE).unwrap();
        assert_eq!(alloc.find_owned(Pid(1), r.base), Some(r));
        assert_eq!(alloc.find_owned(Pid(2), r.base), None);
        assert_eq!(alloc.find_owned(Pid(1), r.base + 4), None);
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
        assert_eq!(alloc.free(Pid(1), whole), Ok(MemoryClass::Sram));

        for _ in 0..ALLOCATIONS {
            alloc.allocate(Pid(1), PAGE).unwrap();
        }
        assert_eq!(alloc.allocate(Pid(1), PAGE), Err(Errno::Enospc));
    }

    #[test]
    fn psram_pool_manages_full_32_mib_and_in_flight_dma_holds() {
        let sram_base = 0x4ff6_0000;
        let psram_start = 0x8800_0000;
        let psram_end = 0x8a00_0000; // full 32 MiB = 512 * 64 KiB pages
        let mut alloc = RegionAllocator::new(sram_base, sram_base + 16 * PAGE);

        // Before PSRAM pool is configured, Psram allocations fail with ENOMEM.
        assert!(!alloc.psram_configured());
        assert_eq!(
            alloc.allocate_in(MemoryClass::Psram, Pid(1), 65536),
            Err(Errno::Enomem)
        );

        alloc.configure_psram(psram_start, psram_end, PSRAM_MMU_PAGE);
        assert!(alloc.psram_configured());
        assert_eq!(alloc.psram_pool().pages(), 512);

        // Without display framebuffer active, a single 32 MiB NAPOT region can be allocated!
        let full_32m = alloc
            .allocate_in(MemoryClass::Psram, Pid(1), 32 * 1024 * 1024)
            .unwrap();
        assert_eq!(
            full_32m,
            Region {
                base: 0x8800_0000,
                size: 32 * 1024 * 1024,
            }
        );
        assert_eq!(alloc.free(Pid(1), full_32m), Ok(MemoryClass::Psram));

        // Allocate a 64 KiB user region at 0x8800_0000, then a 2 MiB display framebuffer;
        // the 2 MiB framebuffer naturally aligns to 0x8820_0000.
        let p1 = alloc
            .allocate_in(MemoryClass::Psram, Pid(1), 16 * 1024)
            .unwrap();
        assert_eq!(
            p1,
            Region {
                base: 0x8800_0000,
                size: 64 * 1024,
            }
        );
        let fb = alloc
            .allocate_in(MemoryClass::Psram, Pid(99), 1_843_200)
            .unwrap();
        assert_eq!(
            fb,
            Region {
                base: 0x8820_0000,
                size: 2 * 1024 * 1024,
            }
        );

        // Mark `fb` as in-flight on DW-GDMA channel 1: cannot be freed while active.
        assert_eq!(
            alloc.set_in_flight(Pid(99), fb, Some(1)),
            Ok(MemoryClass::Psram)
        );
        assert!(alloc.is_in_flight(fb));
        assert_eq!(alloc.free(Pid(99), fb), Err(Errno::Ebusy));

        // If owner exits while DMA is in-flight, region stays reserved until DMA finishes.
        let mut freed_on_exit = 0usize;
        alloc.release_psram_owner_with(Pid(99), |_| freed_on_exit += 1);
        assert_eq!(freed_on_exit, 0);
        assert!(alloc.owns(Pid(99), fb));

        // Once DMA finishes for the exited owner, region is automatically freed.
        assert_eq!(
            alloc.finish_in_flight(fb, 1, false, true),
            Ok((MemoryClass::Psram, true))
        );
        assert!(!alloc.owns(Pid(99), fb));
        assert_eq!(alloc.free(Pid(1), p1), Ok(MemoryClass::Psram));
    }
}
