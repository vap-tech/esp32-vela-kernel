//! Lifetime of one dedicated DMA page. IDs are monotonic process IDs, not slots.
use crate::id::Pid;

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
#[cfg(test)]
mod tests {
    use super::Lease;
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
