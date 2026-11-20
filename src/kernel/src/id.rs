//! Distinct newtypes for kernel identifiers so recyclable table slots,
//! monotonic IDs, capability handles, and uptime ticks cannot be mixed.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct ProcSlot(pub usize);

impl ProcSlot {
    pub const fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Pid(pub u32);

impl Pid {
    pub const NONE: Self = Self(0);
    /// Monotonic owner tag for kernel-owned subsystem allocations (e.g. display framebuffer).
    pub const KERNEL: Self = Self(u32::MAX);

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) if next < Self::KERNEL.0 => Some(Self(next)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct ThreadSlot(pub usize);

impl ThreadSlot {
    pub const fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Tid(pub u32);

impl Tid {
    pub const NONE: Self = Self(0);

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Handle(pub u32);

impl Handle {
    pub const fn raw(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Ticks(pub u64);

impl Ticks {
    pub const ZERO: Self = Self(0);

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn saturating_add(self, delta: u64) -> Self {
        Self(self.0.saturating_add(delta))
    }

    pub const fn saturating_sub(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}
