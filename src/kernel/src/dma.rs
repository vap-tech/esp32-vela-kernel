//! Unified DMA capability table, per-channel `InFlight` region ownership, and
//! cache-coherency alignment/direction rules (`GDMA-AHB`, `DW-GDMA` scanout,
//! and future `SDMMC`/`I2S`). IDs are monotonic process IDs, never slots.
use crate::{
    capability::Object,
    errno::Errno,
    id::{Pid, Ticks},
    memory::MemoryClass,
    task::Region,
};

pub const DMA_CHANNELS: usize = 4;
pub const CACHE_LINE_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaController {
    GdmaAhb,
    DwGdmaAxi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaDirection {
    ToDevice,
    FromDevice,
    MemToMem,
}

impl DmaController {
    /// Return whether this hardware DMA master can directly address `class`.
    pub const fn accepts_memory(self, class: MemoryClass) -> bool {
        match self {
            // Current GDMA-AHB CH1 path targets internal SRAM.
            Self::GdmaAhb => matches!(class, MemoryClass::Sram),
            // DesignWare AXI DMA (DW-GDMA CH0) addresses both internal SRAM and external PSRAM.
            Self::DwGdmaAxi => true,
        }
    }
}

/// Validate DMA transfer region bounds, controller memory class compatibility,
/// and (when `cached` is true for future `L1-DCache` mappings) 64-byte cache-line
/// alignment so clean/invalidate hooks cannot corrupt neighboring data.
pub fn validate_dma_buffer(
    controller: DmaController,
    class: MemoryClass,
    region: Region,
    cached: bool,
) -> Result<(), Errno> {
    if region.size == 0 || !controller.accepts_memory(class) {
        return Err(Errno::Einval);
    }
    if cached && (region.base % CACHE_LINE_BYTES != 0 || region.size % CACHE_LINE_BYTES != 0) {
        return Err(Errno::Einval);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lease {
    Free,
    Cpu(Pid),
    Device(Pid),
    Detached,
    Quarantined,
}

impl Lease {
    pub fn acquire(&mut self, pid: Pid) -> bool {
        if *self != Self::Free {
            return false;
        }
        *self = Self::Cpu(pid);
        true
    }

    pub fn submit(&mut self, pid: Pid) -> bool {
        if *self != Self::Cpu(pid) {
            return false;
        }
        *self = Self::Device(pid);
        true
    }

    pub fn release_process(&mut self, pid: Pid) {
        if *self == Self::Cpu(pid) {
            *self = Self::Free;
        } else if *self == Self::Device(pid) {
            *self = Self::Detached;
        }
    }

    /// A failed completion does not establish that the bus master is quiescent.
    pub fn finish(&mut self, success: bool) -> Option<Pid> {
        let pid = match *self {
            Self::Device(pid) => Some(pid),
            Self::Detached => None,
            _ => return None,
        };
        *self = if success {
            pid.map_or(Self::Free, Self::Cpu)
        } else {
            Self::Quarantined
        };
        pid
    }
}

#[derive(Clone, Copy)]
struct ChannelSlot {
    configured: bool,
    controller: DmaController,
    generation: u32,
    lease: Lease,
    in_flight_region: Option<Region>,
    direction: DmaDirection,
    length: usize,
    deadline: Option<Ticks>,
    completed: u32,
    errors: u32,
    bytes: u64,
}

impl ChannelSlot {
    const EMPTY: Self = Self {
        configured: false,
        controller: DmaController::GdmaAhb,
        generation: 0,
        lease: Lease::Free,
        in_flight_region: None,
        direction: DmaDirection::MemToMem,
        length: 0,
        deadline: None,
        completed: 0,
        errors: 0,
        bytes: 0,
    };
}

pub struct DmaTable {
    slots: [ChannelSlot; DMA_CHANNELS],
}

impl Default for DmaTable {
    fn default() -> Self {
        Self::new()
    }
}

impl DmaTable {
    pub const fn new() -> Self {
        Self {
            slots: [ChannelSlot::EMPTY; DMA_CHANNELS],
        }
    }

    pub fn register(&mut self, index: usize, controller: DmaController) -> Result<(), Errno> {
        if index >= DMA_CHANNELS {
            return Err(Errno::Einval);
        }
        if self.slots[index].configured {
            return Err(Errno::Ebusy);
        }
        self.slots[index] = ChannelSlot {
            configured: true,
            controller,
            ..ChannelSlot::EMPTY
        };
        Ok(())
    }

    pub fn acquire(&mut self, index: usize, owner: Pid) -> Result<Object, Errno> {
        let slot = self.slots.get_mut(index).ok_or(Errno::Einval)?;
        if !slot.configured || owner.is_none() {
            return Err(Errno::Einval);
        }
        if slot.generation >= 0x7f_ffff {
            return Err(Errno::Eoverflow);
        }
        if !slot.lease.acquire(owner) {
            return Err(Errno::Ebusy);
        }
        slot.generation += 1;
        Ok(Object::DmaChannel {
            index,
            generation: slot.generation,
        })
    }

    pub fn resolve(&self, object: Object, owner: Pid) -> Result<usize, Errno> {
        let Object::DmaChannel { index, generation } = object else {
            return Err(Errno::Ebadf);
        };
        let slot = self.slots.get(index).ok_or(Errno::Ebadf)?;
        if !slot.configured || generation == 0 || slot.generation != generation {
            return Err(Errno::Ebadf);
        }
        if slot.lease != Lease::Cpu(owner) && slot.lease != Lease::Device(owner) {
            return Err(Errno::Eperm);
        }
        Ok(index)
    }

    pub fn is_cpu_owner(&self, index: usize, owner: Pid) -> bool {
        self.slots
            .get(index)
            .is_some_and(|s| s.configured && s.lease == Lease::Cpu(owner))
    }

    pub fn is_active(&self, index: usize) -> bool {
        self.slots
            .get(index)
            .is_some_and(|s| s.configured && matches!(s.lease, Lease::Device(_) | Lease::Detached))
    }

    pub fn any_active(&self) -> bool {
        (0..DMA_CHANNELS).any(|i| self.is_active(i))
    }

    pub fn in_flight_region(&self, index: usize) -> Option<Region> {
        self.slots.get(index).and_then(|s| s.in_flight_region)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        &mut self,
        index: usize,
        owner: Pid,
        class: MemoryClass,
        region: Region,
        direction: DmaDirection,
        length: usize,
        deadline: Option<Ticks>,
        cached: bool,
    ) -> Result<(), Errno> {
        let slot = self.slots.get_mut(index).ok_or(Errno::Einval)?;
        if !slot.configured {
            return Err(Errno::Einval);
        }
        validate_dma_buffer(slot.controller, class, region, cached)?;
        if length == 0 || length > region.size {
            return Err(Errno::Einval);
        }
        if !slot.lease.submit(owner) {
            return Err(Errno::Eperm);
        }
        slot.in_flight_region = Some(region);
        slot.direction = direction;
        slot.length = length;
        slot.deadline = deadline;
        Ok(())
    }

    pub fn timed_out(&self, index: usize, uptime: Ticks) -> bool {
        self.slots.get(index).is_some_and(|s| {
            s.configured
                && matches!(s.lease, Lease::Device(_) | Lease::Detached)
                && s.deadline.is_some_and(|d| uptime >= d)
        })
    }

    pub fn length(&self, index: usize) -> usize {
        self.slots.get(index).map_or(0, |s| s.length)
    }

    pub fn finish(&mut self, index: usize, success: bool) -> (Option<Pid>, Option<Region>) {
        let Some(slot) = self.slots.get_mut(index) else {
            return (None, None);
        };
        if !slot.configured {
            return (None, None);
        }
        let region = slot.in_flight_region.take();
        if success {
            slot.completed = slot.completed.saturating_add(1);
            slot.bytes = slot.bytes.saturating_add(slot.length as u64);
        } else {
            slot.errors = slot.errors.saturating_add(1);
        }
        slot.deadline = None;
        let owner = slot.lease.finish(success);
        (owner, region)
    }

    pub fn release_process(&mut self, index: usize, owner: Pid) {
        if let Some(slot) = self.slots.get_mut(index)
            && slot.configured
        {
            slot.lease.release_process(owner);
        }
    }

    pub fn release_all_for_process(&mut self, owner: Pid) {
        for index in 0..DMA_CHANNELS {
            self.release_process(index, owner);
        }
    }

    pub fn completed(&self, index: usize) -> u32 {
        self.slots.get(index).map_or(0, |s| s.completed)
    }

    pub fn errors(&self, index: usize) -> u32 {
        self.slots.get(index).map_or(0, |s| s.errors)
    }

    pub fn bytes(&self, index: usize) -> u64 {
        self.slots.get(index).map_or(0, |s| s.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        id::Pid,
        process::{AddressSpace, Lifecycle, Processes, READ, WRITE},
        task::Region,
    };

    #[test]
    fn late_completion_after_slot_reuse_has_no_recipient() {
        let page = Region {
            base: 0x10000,
            size: 4096,
        };
        for success in [true, false] {
            let mut pool = Processes::new();
            let mut lease = Lease::Free;
            let slot = pool.create(AddressSpace::EMPTY).unwrap();
            let owner = pool.slot_mut(slot);
            let old_pid = owner.id;
            assert!(lease.acquire(old_pid));
            owner.memory.insert(page, READ | WRITE).unwrap();
            owner.memory.dma_access(page, false).unwrap();
            assert!(lease.submit(old_pid));

            lease.release_process(old_pid);
            owner.memory = AddressSpace::EMPTY;
            owner.state = Lifecycle::Free;

            let replacement = pool.create(AddressSpace::EMPTY).unwrap();
            assert_eq!(replacement, slot);
            let new_pid = pool.slot(replacement).id;
            assert_ne!(new_pid, old_pid);
            assert!(!lease.acquire(new_pid));

            // This is the completion decision consumed by the IRQ/timeout path.
            // None prevents both restoring a mapping and waking a thread.
            assert_eq!(lease.finish(success), None);
            assert!(
                !pool
                    .slot(replacement)
                    .memory
                    .permits(page.base, page.size, READ)
            );
            assert_eq!(lease.acquire(new_pid), success);
        }
    }

    #[test]
    fn exit_waits_for_completion_before_reuse() {
        let mut lease = Lease::Free;
        assert!(lease.acquire(Pid(3)));
        assert!(!lease.submit(Pid(4)));
        assert!(lease.submit(Pid(3)));
        lease.release_process(Pid(3));
        assert!(!lease.acquire(Pid(4)));
        assert_eq!(lease.finish(true), None);
        assert!(lease.acquire(Pid(4)));
        assert!(lease.submit(Pid(4)));
        assert_eq!(lease.finish(false), Some(Pid(4)));
        lease.release_process(Pid(4));
        assert!(!lease.acquire(Pid(5)));
    }

    #[test]
    fn dma_table_enforces_controller_memory_class_cache_alignment_and_generations() {
        let mut dma = DmaTable::new();
        dma.register(0, DmaController::GdmaAhb).unwrap();
        dma.register(1, DmaController::DwGdmaAxi).unwrap();
        assert_eq!(dma.register(0, DmaController::GdmaAhb), Err(Errno::Ebusy));

        let cap0 = dma.acquire(0, Pid(2)).unwrap();
        assert_eq!(dma.resolve(cap0, Pid(2)), Ok(0));
        assert_eq!(dma.resolve(cap0, Pid(3)), Err(Errno::Eperm));

        let sram_reg = Region {
            base: 0x4ff8_0000,
            size: 4096,
        };
        let psram_reg = Region {
            base: 0x8800_0000,
            size: 2 * 1024 * 1024,
        };

        // GDMA-AHB rejects PSRAM regions, accepts SRAM regions.
        assert_eq!(
            dma.submit(
                0,
                Pid(2),
                MemoryClass::Psram,
                psram_reg,
                DmaDirection::MemToMem,
                2048,
                Some(Ticks(100)),
                false,
            ),
            Err(Errno::Einval)
        );
        // Cached buffer validation enforces 64-byte cache line alignment.
        assert_eq!(
            validate_dma_buffer(
                DmaController::DwGdmaAxi,
                MemoryClass::Psram,
                Region {
                    base: 0x8800_0020,
                    size: 4096,
                },
                true,
            ),
            Err(Errno::Einval)
        );

        assert_eq!(
            dma.submit(
                0,
                Pid(2),
                MemoryClass::Sram,
                sram_reg,
                DmaDirection::MemToMem,
                2048,
                Some(Ticks(100)),
                true,
            ),
            Ok(())
        );
        assert!(dma.is_active(0));
        assert_eq!(dma.in_flight_region(0), Some(sram_reg));
        assert!(!dma.timed_out(0, Ticks(99)));
        assert!(dma.timed_out(0, Ticks(100)));

        let (done_owner, done_reg) = dma.finish(0, true);
        assert_eq!(done_owner, Some(Pid(2)));
        assert_eq!(done_reg, Some(sram_reg));
        assert_eq!(dma.completed(0), 1);
        assert_eq!(dma.bytes(0), 2048);

        dma.release_process(0, Pid(2));
        let next_cap = dma.acquire(0, Pid(3)).unwrap();
        assert_ne!(next_cap, cap0);
        assert_eq!(dma.resolve(cap0, Pid(2)), Err(Errno::Ebadf));
    }
}

#[cfg(test)]
mod mapping_tests {
    use crate::{
        process::{AddressSpace, READ, WRITE},
        task::Region,
    };

    #[test]
    fn device_owned_page_retains_its_slot_but_denies_access() {
        let mut space = AddressSpace::EMPTY;
        let page = Region {
            base: 0x10000,
            size: 4096,
        };
        space.insert(page, READ | WRITE).unwrap();
        space.dma_access(page, false).unwrap();
        assert!(!space.permits(page.base, 1, READ));
        assert!(!space.permits(page.base, 1, WRITE));
        assert!(space.insert(page, READ | WRITE).is_err());
        space.dma_access(page, true).unwrap();
        assert!(space.permits(page.base, page.size, READ | WRITE));
    }
}
