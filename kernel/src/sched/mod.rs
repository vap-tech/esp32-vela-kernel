use crate::{
    stats::{HZ, LoadAverage, Snapshot},
    task::{Region, State, Task},
};

/// Static two-slot round robin. Only accessed by hart0 with machine interrupts
/// disabled; all context switches happen at the outermost user trap boundary.
/// C is architecture-owned saved CPU state; this crate has no SoC dependency.
pub struct Scheduler<C: Copy> {
    pub tasks: [Task<C>; 2],
    pub current: usize,
    ticks: u64,
    uptime: u64,
    sampled_at: u64,
    last_id: u32,
    load: LoadAverage,
}
impl<C: Copy> Scheduler<C> {
    pub fn new(contexts: [C; 2], entry: usize, code: Region, stacks: [Region; 2]) -> Self {
        let tasks = core::array::from_fn(|i| Task {
            id: i as u32 + 1,
            state: if i == 0 { State::Running } else { State::Ready },
            context: contexts[i],
            entry,
            code,
            stack: stacks[i],
            runtime_ticks: 0,
            preemptions: 0,
        });
        Self {
            tasks,
            current: 0,
            ticks: 0,
            uptime: 0,
            sampled_at: 0,
            last_id: 2,
            load: LoadAverage::default(),
        }
    }
    pub fn current(&self) -> &Task<C> {
        &self.tasks[self.current]
    }
    pub fn save(&mut self, context: C) {
        self.tasks[self.current].context = context;
    }
    pub fn tick(&mut self, uptime: u64) {
        self.ticks += 1;
        self.tasks[self.current].runtime_ticks += uptime.saturating_sub(self.uptime);
        self.uptime = uptime;
        while self.uptime.saturating_sub(self.sampled_at) >= 5 * HZ {
            self.load.sample(self.snapshot().runnable_tasks);
            self.sampled_at += 5 * HZ;
        }
    }
    pub fn select(&mut self, preemptive: bool) -> Option<usize> {
        let was_running = self.tasks[self.current].state == State::Running;
        if was_running {
            self.tasks[self.current].state = State::Ready;
        }
        for offset in 1..=self.tasks.len() {
            let next = (self.current + offset) % self.tasks.len();
            if self.tasks[next].state == State::Ready {
                if preemptive && was_running && next != self.current {
                    self.tasks[self.current].preemptions += 1;
                }
                self.tasks[next].state = State::Running;
                self.current = next;
                return Some(next);
            }
        }
        None
    }
    pub fn terminate(&mut self) {
        self.tasks[self.current].state = State::Terminated;
    }
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            total_tasks: self
                .tasks
                .iter()
                .filter(|t| t.state != State::Terminated)
                .count() as u32,
            runnable_tasks: self
                .tasks
                .iter()
                .filter(|t| matches!(t.state, State::Ready | State::Running))
                .count() as u32,
            running_tasks: self
                .tasks
                .iter()
                .filter(|t| t.state == State::Running)
                .count() as u32,
            last_task_id: self.last_id,
            scheduler_ticks: self.ticks,
            uptime_ticks: self.uptime,
            load_q16: self.load.values(),
        }
    }
}
