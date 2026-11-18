use crate::{
    stats::{HZ, LoadAverage, Snapshot},
    task::{Region, State, Thread, Wait},
};

pub const TASKS: usize = 8;
pub const BOOT_THREADS: usize = 3;

/// Static three-slot round robin. Only accessed by hart0 with machine interrupts
/// disabled; all context switches happen at the outermost user trap boundary.
/// C is architecture-owned saved CPU state; this crate has no SoC dependency.
pub struct Scheduler<C: Copy> {
    pub tasks: [Thread<C>; TASKS],
    pub current: usize,
    ticks: u64,
    uptime: u64,
    sampled_at: u64,
    last_id: u32,
    load: LoadAverage,
}
impl<C: Copy> Scheduler<C> {
    pub fn new(
        contexts: [C; BOOT_THREADS],
        entries: [usize; BOOT_THREADS],
        stacks: [Region; BOOT_THREADS],
    ) -> Self {
        let tasks = core::array::from_fn(|i| Thread {
            id: if i < BOOT_THREADS { i as u32 + 1 } else { 0 },
            wait: Wait::None,
            state: if i >= BOOT_THREADS {
                State::Terminated
            } else if i == 0 {
                State::Running
            } else {
                State::Ready
            },
            context: contexts[i.min(BOOT_THREADS - 1)],
            entry: entries[i.min(BOOT_THREADS - 1)],
            process: i,
            priority: 1,
            effective_priority: 1,
            affinity: 1,
            stack: stacks[i.min(BOOT_THREADS - 1)],
            runtime_ticks: 0,
            preemptions: 0,
        });
        Self {
            tasks,
            current: 0,
            ticks: 0,
            uptime: 0,
            sampled_at: 0,
            last_id: BOOT_THREADS as u32,
            load: LoadAverage::default(),
        }
    }
    pub fn create_thread(
        &mut self,
        process: usize,
        context: C,
        stack: Region,
        entry: usize,
        priority: u8,
        affinity: u8,
    ) -> Result<u32, u32> {
        if priority > 3 || affinity == 0 || affinity & !3 != 0 {
            return Err(22);
        }
        let index = self
            .tasks
            .iter()
            .position(|t| t.state == State::Terminated)
            .ok_or(28u32)?;
        let id = self.last_id.checked_add(1).ok_or(75u32)?;
        self.tasks[index] = Thread {
            id,
            process,
            context,
            stack,
            entry,
            priority,
            effective_priority: priority,
            affinity,
            wait: Wait::None,
            state: State::Ready,
            runtime_ticks: 0,
            preemptions: 0,
        };
        self.last_id = id;
        Ok(id)
    }
    pub fn current(&self) -> &Thread<C> {
        &self.tasks[self.current]
    }
    pub fn save(&mut self, context: C) {
        self.tasks[self.current].context = context;
    }
    pub fn tick(&mut self, uptime: u64) {
        self.ticks += 1;
        if self.tasks[self.current].state == State::Running {
            self.tasks[self.current].runtime_ticks += uptime.saturating_sub(self.uptime);
        }
        for task in &mut self.tasks {
            if task.state == State::Blocked
                && matches!(task.wait, Wait::Sleep { until } if until <= uptime)
            {
                task.wait = Wait::None;
                task.state = State::Ready;
            }
        }
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
    pub fn block(&mut self, wait: Wait) {
        self.tasks[self.current].wait = wait;
        self.tasks[self.current].state = State::Blocked;
    }
    pub fn wake(&mut self, task: usize) {
        if self.tasks[task].state == State::Blocked {
            self.tasks[task].wait = Wait::None;
            self.tasks[task].state = State::Ready;
        }
    }
    pub fn terminate(&mut self) {
        self.tasks[self.current].wait = Wait::None;
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
