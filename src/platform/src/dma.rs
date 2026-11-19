use vela_kernel::{
    dma::Lease,
    id::{Pid, Ticks},
};

pub struct DmaChannel {
    lease: Lease,
    length: usize,
    deadline: Ticks,
    completed: u32,
    errors: u32,
    bytes: u64,
}

impl DmaChannel {
    pub const fn new() -> Self {
        Self {
            lease: Lease::Free,
            length: 0,
            deadline: Ticks::ZERO,
            completed: 0,
            errors: 0,
            bytes: 0,
        }
    }

    pub fn acquire(&mut self, owner: Pid) -> bool {
        self.lease.acquire(owner)
    }

    pub fn release_process(&mut self, owner: Pid) {
        self.lease.release_process(owner);
    }

    pub fn is_cpu_owner(&self, owner: Pid) -> bool {
        self.lease == Lease::Cpu(owner)
    }

    pub fn submit(&mut self, owner: Pid, length: usize, deadline: Ticks) {
        assert!(self.lease.submit(owner));
        self.length = length;
        self.deadline = deadline;
    }

    pub fn timed_out(&self, uptime: Ticks) -> bool {
        matches!(self.lease, Lease::Device(_) | Lease::Detached) && uptime >= self.deadline
    }

    pub fn length(&self) -> usize {
        self.length
    }

    pub fn finish(&mut self, success: bool) -> Option<Pid> {
        if success {
            self.completed = self.completed.saturating_add(1);
            self.bytes = self.bytes.saturating_add(self.length as u64);
        } else {
            self.errors = self.errors.saturating_add(1);
        }
        self.lease.finish(success)
    }

    pub const fn completed(&self) -> u32 {
        self.completed
    }

    pub const fn errors(&self) -> u32 {
        self.errors
    }

    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}
