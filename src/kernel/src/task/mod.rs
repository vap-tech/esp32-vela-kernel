use crate::{
    capability::Object,
    id::{Handle, ProcSlot, ThreadSlot, Ticks, Tid},
};

/// Architecture-owned saved CPU state: the only view the kernel crate has of
/// the syscall return registers (a0, a1).
pub trait Context: Copy {
    fn set_return(&mut self, value: u32);
    fn set_return2(&mut self, value: u32);
}

/// Half-open user mapping. No overflow or cross-region syscall buffers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Region {
    pub base: usize,
    pub size: usize,
}
impl Region {
    pub fn contains(self, pointer: usize, length: usize) -> bool {
        pointer >= self.base
            && pointer.checked_add(length).is_some_and(|end| {
                self.base
                    .checked_add(self.size)
                    .is_some_and(|limit| end <= limit)
            })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Ready,
    Running,
    Blocked,
    Terminated,
}

/// Platform-independent wait reasons. Timeouts live uniformly on
/// [`Thread::deadline`] rather than per-variant fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    None,
    Sleep,
    Receive {
        endpoint: usize,
        handle: Handle,
        pointer: usize,
        capacity: usize,
    },
    Reply {
        token: u32,
        pointer: usize,
        capacity: usize,
        server: ThreadSlot,
    },
    Event {
        object: Object,
    },
    Device {
        domain: u8,
        token: u32,
        pointer: usize,
    },
}

#[derive(Clone, Copy)]
pub struct Thread<C: Copy> {
    pub id: Tid,
    pub wait: Wait,
    pub deadline: Option<Ticks>,
    pub state: State,
    pub context: C,
    pub entry: usize,
    pub process: ProcSlot,
    pub priority: u8,
    pub effective_priority: u8,
    pub affinity: u8,
    pub stack: Region,
    pub runtime_ticks: u64,
    pub preemptions: u64,
}
