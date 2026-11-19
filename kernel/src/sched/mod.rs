use crate::{
    errno::Errno,
    id::{ProcSlot, ThreadSlot, Ticks, Tid},
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
    pub current: ThreadSlot,
    ticks: u64,
    uptime: Ticks,
    sampled_at: Ticks,
    last_id: Tid,
    load: LoadAverage,
    preferred: Option<ThreadSlot>,
}
impl<C: Copy> Scheduler<C> {
    pub fn new(
        contexts: [C; BOOT_THREADS],
        entries: [usize; BOOT_THREADS],
        stacks: [Region; BOOT_THREADS],
    ) -> Self {
        let tasks = core::array::from_fn(|i| Thread {
            id: if i < BOOT_THREADS {
                Tid(i as u32 + 1)
            } else {
                Tid::NONE
            },
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
            process: ProcSlot(i),
            priority: 1,
            effective_priority: 1,
            affinity: 1,
            stack: stacks[i.min(BOOT_THREADS - 1)],
            runtime_ticks: 0,
            preemptions: 0,
        });
        Self {
            tasks,
            current: ThreadSlot(0),
            ticks: 0,
            uptime: Ticks::ZERO,
            sampled_at: Ticks::ZERO,
            last_id: Tid(BOOT_THREADS as u32),
            load: LoadAverage::default(),
            preferred: None,
        }
    }
    pub fn task(&self, slot: ThreadSlot) -> &Thread<C> {
        &self.tasks[slot.index()]
    }
    pub fn task_mut(&mut self, slot: ThreadSlot) -> &mut Thread<C> {
        &mut self.tasks[slot.index()]
    }
    pub fn uptime(&self) -> Ticks {
        self.uptime
    }
    pub fn create_thread(
        &mut self,
        process: ProcSlot,
        context: C,
        stack: Region,
        entry: usize,
        priority: u8,
        affinity: u8,
    ) -> Result<Tid, Errno> {
        if priority > 3 || affinity == 0 || affinity & !3 != 0 {
            return Err(Errno::Einval);
        }
        let index = self
            .tasks
            .iter()
            .position(|t| t.state == State::Terminated)
            .ok_or(Errno::Enospc)?;
        let id = self.last_id.checked_next().ok_or(Errno::Eoverflow)?;
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
        &self.tasks[self.current.index()]
    }
    pub fn save(&mut self, context: C) {
        self.tasks[self.current.index()].context = context;
    }
    pub fn tick(&mut self, uptime: Ticks) {
        self.ticks += 1;
        let cur = self.current.index();
        if self.tasks[cur].state == State::Running {
            self.tasks[cur].runtime_ticks += uptime.saturating_sub(self.uptime);
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
            self.sampled_at = self.sampled_at.saturating_add(5 * HZ);
        }
    }
    pub fn select(&mut self, preemptive: bool) -> Option<ThreadSlot> {
        let cur = self.current.index();
        let was_running = self.tasks[cur].state == State::Running;
        if was_running {
            self.tasks[cur].state = State::Ready;
        }
        self.inherit();
        let best = self
            .tasks
            .iter()
            .filter(|t| t.state == State::Ready && t.affinity & 1 != 0)
            .map(|t| t.effective_priority)
            .max();
        if let Some(preferred) = self.preferred.take() {
            let idx = preferred.index();
            let t = &self.tasks[idx];
            if t.state == State::Ready && t.affinity & 1 != 0 && Some(t.effective_priority) == best
            {
                self.tasks[idx].state = State::Running;
                self.current = preferred;
                return Some(preferred);
            }
        }
        for offset in 1..=self.tasks.len() {
            let next = (cur + offset) % self.tasks.len();
            if self.tasks[next].state == State::Ready
                && self.tasks[next].affinity & 1 != 0
                && Some(self.tasks[next].effective_priority) == best
            {
                if preemptive && was_running && next != cur {
                    self.tasks[cur].preemptions += 1;
                }
                self.tasks[next].state = State::Running;
                self.current = ThreadSlot(next);
                return Some(ThreadSlot(next));
            }
        }
        None
    }
    pub fn prefer(&mut self, thread: ThreadSlot) {
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
                    let srv = server.index();
                    if srv < TASKS
                        && self.tasks[srv].state != State::Terminated
                        && self.tasks[srv].effective_priority < priority
                    {
                        self.tasks[srv].effective_priority = priority;
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
        let cur = self.current.index();
        self.tasks[cur].wait = wait;
        self.tasks[cur].state = State::Blocked;
    }
    pub fn wake(&mut self, task: ThreadSlot) {
        let idx = task.index();
        if self.tasks[idx].state == State::Blocked {
            self.tasks[idx].wait = Wait::None;
            self.tasks[idx].state = State::Ready;
        }
    }
    pub fn terminate(&mut self) {
        let cur = self.current.index();
        self.tasks[cur].wait = Wait::None;
        self.tasks[cur].state = State::Terminated;
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
            last_task_id: self.last_id.raw(),
            scheduler_ticks: self.ticks,
            uptime_ticks: self.uptime.raw(),
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
        assert_eq!(s.select(true), Some(ThreadSlot(1)));
        assert_eq!(s.tasks[0].preemptions, 1);
        assert_eq!(s.select(false), Some(ThreadSlot(2)));
        assert_eq!(s.tasks[1].preemptions, 0);

        // Preferred thread wins when tied at the highest ready priority.
        s.prefer(ThreadSlot(0));
        assert_eq!(s.select(false), Some(ThreadSlot(0)));

        // Higher priority always beats lower preferred thread.
        s.tasks[2].priority = 3;
        s.tasks[2].effective_priority = 3;
        assert!(s.should_preempt());
        s.prefer(ThreadSlot(1));
        assert_eq!(s.select(true), Some(ThreadSlot(2)));
    }

    #[test]
    fn transitive_priority_inheritance_boosts_chain_and_restores_after_wake() {
        let mut s = test_scheduler();
        s.tasks[0].priority = 0; // server C
        s.tasks[1].priority = 1; // middle B
        s.tasks[2].priority = 3; // high-priority client A

        // B (1) waits on C (0), A (2) waits on B (1).
        s.current = ThreadSlot(1);
        s.tasks[1].state = State::Running;
        s.block(Wait::Reply {
            token: 1,
            pointer: 0,
            capacity: 0,
            server: ThreadSlot(0),
            until: None,
        });
        s.current = ThreadSlot(2);
        s.tasks[2].state = State::Running;
        s.block(Wait::Reply {
            token: 2,
            pointer: 0,
            capacity: 0,
            server: ThreadSlot(1),
            until: None,
        });

        s.inherit();
        assert_eq!(s.tasks[1].effective_priority, 3);
        assert_eq!(s.tasks[0].effective_priority, 3);

        s.wake(ThreadSlot(2));
        s.inherit();
        assert_eq!(s.tasks[1].effective_priority, 1);
        assert_eq!(s.tasks[0].effective_priority, 1);
    }

    #[test]
    fn tick_wakes_sleepers_tracks_runtime_and_samples_load() {
        let mut s = test_scheduler();
        s.block(Wait::Sleep { until: Ticks(10) });
        assert_eq!(s.select(false), Some(ThreadSlot(1)));
        s.tick(Ticks(5));
        assert_eq!(s.tasks[0].state, State::Blocked);
        assert_eq!(s.tasks[1].runtime_ticks, 5);

        s.tick(Ticks(10));
        assert_eq!(s.tasks[0].state, State::Ready);

        // Advance past 5-second sampling window.
        s.tick(Ticks(5 * HZ));
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
        assert_eq!(
            s.create_thread(ProcSlot(0), 0, stack, 0x400, 4, 1),
            Err(Errno::Einval)
        );
        assert_eq!(
            s.create_thread(ProcSlot(0), 0, stack, 0x400, 1, 0),
            Err(Errno::Einval)
        );
        assert_eq!(
            s.create_thread(ProcSlot(0), 0, stack, 0x400, 1, 4),
            Err(Errno::Einval)
        );

        for i in 0..(TASKS - BOOT_THREADS) {
            let tid = s.create_thread(ProcSlot(0), 0, stack, 0x400, 1, 1).unwrap();
            assert_eq!(tid, Tid(BOOT_THREADS as u32 + i as u32 + 1));
        }
        assert_eq!(
            s.create_thread(ProcSlot(0), 0, stack, 0x400, 1, 1),
            Err(Errno::Enospc)
        );
    }
}
