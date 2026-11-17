/// Half-open user mapping. No overflow or cross-region syscall buffers.
#[derive(Clone, Copy, Debug, Default)]
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
#[derive(Clone, Copy)]
pub struct Task<C: Copy> {
    pub id: u32,
    pub state: State,
    pub context: C,
    pub entry: usize,
    pub code: Region,
    pub stack: Region,
    pub runtime_ticks: u64,
    pub preemptions: u64,
}
impl<C: Copy> Task<C> {
    pub fn readable(&self, pointer: usize, length: usize) -> bool {
        self.code.contains(pointer, length) || self.stack.contains(pointer, length)
    }
}
