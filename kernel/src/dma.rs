//! Lifetime of one dedicated DMA page. IDs are monotonic process IDs, not slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lease {
    Free,
    Cpu(u32),
    Device(u32),
    Detached,
    Quarantined,
}
impl Lease {
    pub fn acquire(&mut self, pid: u32) -> bool {
        if *self != Self::Free {
            return false;
        }
        *self = Self::Cpu(pid);
        true
    }
    pub fn submit(&mut self, pid: u32) -> bool {
        if *self != Self::Cpu(pid) {
            return false;
        }
        *self = Self::Device(pid);
        true
    }
    pub fn release_process(&mut self, pid: u32) {
        if *self == Self::Cpu(pid) {
            *self = Self::Free;
        } else if *self == Self::Device(pid) {
            *self = Self::Detached;
        }
    }
    /// A failed completion does not establish that the bus master is quiescent.
    pub fn finish(&mut self, success: bool) -> Option<u32> {
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
    #[test]
    fn exit_waits_for_completion_before_reuse() {
        let mut lease = Lease::Free;
        assert!(lease.acquire(3));
        assert!(!lease.submit(4));
        assert!(lease.submit(3));
        lease.release_process(3);
        assert!(!lease.acquire(4));
        assert_eq!(lease.finish(true), None);
        assert!(lease.acquire(4));
        assert!(lease.submit(4));
        assert_eq!(lease.finish(false), Some(4));
        lease.release_process(4);
        assert!(!lease.acquire(5));
    }
}
