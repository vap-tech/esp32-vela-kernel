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
    preferred: Option<usize>,
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
            preferred: None,
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
        self.inherit();
        let best = self
            .tasks
            .iter()
            .filter(|t| t.state == State::Ready && t.affinity & 1 != 0)
            .map(|t| t.effective_priority)
            .max();
        if let Some(preferred) = self.preferred.take() {
            let t = &self.tasks[preferred];
            if t.state == State::Ready && t.affinity & 1 != 0 && Some(t.effective_priority) == best
            {
                self.tasks[preferred].state = State::Running;
                self.current = preferred;
                return Some(preferred);
            }
        }
        for offset in 1..=self.tasks.len() {
            let next = (self.current + offset) % self.tasks.len();
            if self.tasks[next].state == State::Ready
                && self.tasks[next].affinity & 1 != 0
                && Some(self.tasks[next].effective_priority) == best
            {
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
    pub fn prefer(&mut self, thread: usize) {
        self.preferred = Some(thread);
    }
    pub fn should_preempt(&self) -> bool {
        self.tasks.iter().any(|t| {
            t.state == State::Ready
                && t.affinity & 1 != 0
                && t.effective_priority > self.current().effective_priority
        })
    }
    pub fn inherit(&mut self) {
        for t in &mut self.tasks {
            t.effective_priority = t.priority;
        }
        // At most TASKS dependency edges. Bounded transitive donation; no heap.
        for _ in 0..TASKS {
            let mut changed = false;
            for i in 0..TASKS {
                if self.tasks[i].state != State::Blocked {
                    continue;
                }
                if let Wait::Reply { server, .. } = self.tasks[i].wait {
                    let priority = self.tasks[i].effective_priority;
                    if server < TASKS
                        && self.tasks[server].state != State::Terminated
                        && self.tasks[server].effective_priority < priority
                    {
                        self.tasks[server].effective_priority = priority;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_scheduler() -> Scheduler<u32> {
        let stacks = [
            Region {
                base: 0x1000,
                size: 4096,
            },
            Region {
                base: 0x2000,
                size: 4096,
            },
            Region {
                base: 0x3000,
                size: 4096,
            },
        ];
        Scheduler::new([0, 1, 2], [0x100, 0x200, 0x300], stacks)
    }

    #[test]
    fn priority_round_robin_and_preferred_tie_break() {
        let mut s = test_scheduler();
        assert_eq!(s.select(true), Some(1));
        assert_eq!(s.tasks[0].preemptions, 1);
        assert_eq!(s.select(false), Some(2));
        assert_eq!(s.tasks[1].preemptions, 0);

        // Preferred thread wins when tied at the highest ready priority.
        s.prefer(0);
        assert_eq!(s.select(false), Some(0));

        // Higher priority always beats lower preferred thread.
        s.tasks[2].priority = 3;
        s.tasks[2].effective_priority = 3;
        assert!(s.should_preempt());
        s.prefer(1);
        assert_eq!(s.select(true), Some(2));
    }

    #[test]
    fn transitive_priority_inheritance_boosts_chain_and_restores_after_wake() {
        let mut s = test_scheduler();
        s.tasks[0].priority = 0; // server C
        s.tasks[1].priority = 1; // middle B
        s.tasks[2].priority = 3; // high-priority client A

        // B (1) waits on C (0), A (2) waits on B (1).
        s.current = 1;
        s.tasks[1].state = State::Running;
        s.block(Wait::Reply {
            token: 1,
            pointer: 0,
            capacity: 0,
            server: 0,
            until: 0,
        });
        s.current = 2;
        s.tasks[2].state = State::Running;
        s.block(Wait::Reply {
            token: 2,
            pointer: 0,
            capacity: 0,
            server: 1,
            until: 0,
        });

        s.inherit();
        assert_eq!(s.tasks[1].effective_priority, 3);
        assert_eq!(s.tasks[0].effective_priority, 3);

        s.wake(2);
        s.inherit();
        assert_eq!(s.tasks[1].effective_priority, 1);
        assert_eq!(s.tasks[0].effective_priority, 1);
    }

    #[test]
    fn tick_wakes_sleepers_tracks_runtime_and_samples_load() {
        let mut s = test_scheduler();
        s.block(Wait::Sleep { until: 10 });
        assert_eq!(s.select(false), Some(1));
        s.tick(5);
        assert_eq!(s.tasks[0].state, State::Blocked);
        assert_eq!(s.tasks[1].runtime_ticks, 5);

        s.tick(10);
        assert_eq!(s.tasks[0].state, State::Ready);

        // Advance past 5-second sampling window.
        s.tick(5 * HZ);
        let snap = s.snapshot();
        assert_eq!(snap.uptime_ticks, 5 * HZ);
        assert!(snap.load_q16[0] > 0);
        assert_eq!(snap.runnable_tasks, 3);
    }

    #[test]
    fn create_thread_validates_args_and_fills_terminated_slots() {
        let mut s = test_scheduler();
        let stack = Region {
            base: 0x4000,
            size: 4096,
        };
        assert_eq!(s.create_thread(0, 0, stack, 0x400, 4, 1), Err(22));
        assert_eq!(s.create_thread(0, 0, stack, 0x400, 1, 0), Err(22));
        assert_eq!(s.create_thread(0, 0, stack, 0x400, 1, 4), Err(22));

        for i in 0..(TASKS - BOOT_THREADS) {
            let tid = s.create_thread(0, 0, stack, 0x400, 1, 1).unwrap();
            assert_eq!(tid, BOOT_THREADS as u32 + i as u32 + 1);
        }
        assert_eq!(s.create_thread(0, 0, stack, 0x400, 1, 1), Err(28));
    }
}
