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
#[derive(Clone, Copy, Debug)]
pub enum Wait {
    Sentinel {
        sequence: u32,
        deadline: u64,
        pointer: usize,
    },
    Dma,
    Event {
        object: crate::capability::Object,
    },
    None,
    Receive {
        endpoint: usize,
        handle: u32,
        pointer: usize,
        capacity: usize,
    },
    Reply {
        token: u32,
        pointer: usize,
        capacity: usize,
        server: usize,
        until: u64,
    },
    Sleep {
        until: u64,
    },
}
#[derive(Clone, Copy)]
pub struct Thread<C: Copy> {
    pub id: u32,
    pub wait: Wait,
    pub state: State,
    pub context: C,
    pub entry: usize,
    pub process: usize,
    pub priority: u8,
    pub effective_priority: u8,
    pub affinity: u8,
    pub stack: Region,
    pub runtime_ticks: u64,
    pub preemptions: u64,
}
