use crate::{
    call::CallTable,
    errno::Errno,
    id::{ProcSlot, ThreadSlot, Ticks, Tid},
    stats::{HZ, LoadAverage, Snapshot},
    task::{Context, Region, State, Thread, Wait},
};

pub const TASKS: usize = 8;
pub const BOOT_THREADS: usize = 3;
pub const HARTS: usize = 2;

/// Static multi-hart round-robin + priority scheduler. Protected by the kernel
/// spinlock with machine interrupts disabled; all context switches happen at
/// the outermost trap boundary.
/// `C` is architecture-owned saved CPU state; this crate has no SoC dependency.
pub struct Scheduler<C: Copy> {
    pub tasks: [Thread<C>; TASKS],
    pub current: ThreadSlot,
    pub active_hart: usize,
    pub hart_current: [Option<ThreadSlot>; HARTS],
    pub hart_last: [ThreadSlot; HARTS],
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
            deadline: None,
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
            last_hart: 0,
            migrations: 0,
            stack: stacks[i.min(BOOT_THREADS - 1)],
            runtime_ticks: 0,
            user_cycles: 0,
            preemptions: 0,
        });
        Self {
            tasks,
            current: ThreadSlot(0),
            active_hart: 0,
            hart_current: [Some(ThreadSlot(0)), None],
            hart_last: [ThreadSlot(0), ThreadSlot(0)],
            ticks: 0,
            uptime: Ticks::ZERO,
            sampled_at: Ticks::ZERO,
            last_id: Tid(BOOT_THREADS as u32),
            load: LoadAverage::default(),
            preferred: None,
        }
    }
    pub fn empty(empty_context: C) -> Self {
        let tasks = [Thread {
            id: Tid::NONE,
            wait: Wait::None,
            deadline: None,
            state: State::Terminated,
            context: empty_context,
            entry: 0,
            process: ProcSlot(0),
            priority: 1,
            effective_priority: 1,
            affinity: 1,
            last_hart: 0,
            migrations: 0,
            stack: Region { base: 0, size: 0 },
            runtime_ticks: 0,
            user_cycles: 0,
            preemptions: 0,
        }; TASKS];
        Self {
            tasks,
            current: ThreadSlot(0),
            active_hart: 0,
            hart_current: [None; HARTS],
            hart_last: [ThreadSlot(0); HARTS],
            ticks: 0,
            uptime: Ticks::ZERO,
            sampled_at: Ticks::ZERO,
            last_id: Tid::NONE,
            load: LoadAverage::default(),
            preferred: None,
        }
    }
    pub fn activate_initial(&mut self, slot: ThreadSlot) {
        self.activate_initial_on(0, slot);
    }
    pub fn activate_initial_on(&mut self, hart: usize, slot: ThreadSlot) {
        assert!(hart < HARTS);
        assert_eq!(self.tasks[slot.index()].state, State::Ready);
        self.tasks[slot.index()].state = State::Running;
        self.tasks[slot.index()].last_hart = hart as u8;
        self.hart_current[hart] = Some(slot);
        self.hart_last[hart] = slot;
        self.active_hart = hart;
        self.current = slot;
    }
    /// Synchronize the active caller hart (`0` or `1`) on kernel entry so
    /// [`Self::current`] reflects the thread currently executing on `hart`.
    pub fn enter_hart(&mut self, hart: usize) {
        assert!(hart < HARTS);
        self.active_hart = hart;
        self.current = self.hart_current[hart].unwrap_or(self.hart_last[hart]);
    }
    /// Return the hart index (`0..HARTS`) currently executing `slot`, if any.
    pub fn running_hart(&self, slot: ThreadSlot) -> Option<usize> {
        self.hart_current.iter().position(|&cur| cur == Some(slot))
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
    /// Return `true` if any thread is currently `Ready` or `Running` for `self.active_hart`.
    pub fn has_runnable(&self) -> bool {
        self.has_runnable_for(self.active_hart)
    }
    /// Return `true` if any thread is currently `Ready` for `hart` or already running on `hart`.
    pub fn has_runnable_for(&self, hart: usize) -> bool {
        assert!(hart < HARTS);
        if self
            .hart_current
            .get(hart)
            .copied()
            .flatten()
            .is_some_and(|slot| self.tasks[slot.index()].state == State::Running)
        {
            return true;
        }
        let mask = 1u8 << hart;
        self.tasks
            .iter()
            .any(|t| t.state == State::Ready && t.affinity & mask != 0)
    }
    /// Earliest expiration deadline among all `Blocked` threads, if any.
    pub fn nearest_deadline(&self) -> Option<Ticks> {
        self.tasks
            .iter()
            .filter(|t| t.state == State::Blocked)
            .filter_map(|t| t.deadline)
            .min()
    }
    /// Next 5-second (`5 * HZ`) load average sampling boundary.
    pub fn next_load_sample_at(&self) -> Ticks {
        self.sampled_at.saturating_add(5 * HZ)
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
        let initial_hart = if affinity & 1 != 0 { 0 } else { 1 };
        self.tasks[index] = Thread {
            id,
            process,
            context,
            stack,
            entry,
            priority,
            effective_priority: priority,
            affinity,
            last_hart: initial_hart,
            migrations: 0,
            wait: Wait::None,
            deadline: None,
            state: State::Ready,
            runtime_ticks: 0,
            user_cycles: 0,
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
    pub fn select(&mut self, preemptive: bool) -> Option<ThreadSlot> {
        self.select_for(self.active_hart, preemptive)
    }
    /// Select the next runnable thread eligible for `hart` (`affinity & (1 << hart) != 0`).
    ///
    /// A thread currently `State::Running` on another hart remains `State::Running`
    /// and is never considered here, guaranteeing a thread context cannot execute
    /// on two harts simultaneously.
    pub fn select_for(&mut self, hart: usize, preemptive: bool) -> Option<ThreadSlot> {
        assert!(hart < HARTS);
        self.active_hart = hart;
        let mask = 1u8 << hart;
        let cur_slot = self.hart_current[hart].unwrap_or_else(|| {
            if self.current.index() < TASKS
                && (0..HARTS).all(|h| h == hart || self.hart_current[h] != Some(self.current))
            {
                self.current
            } else {
                self.hart_last[hart]
            }
        });
        let cur = cur_slot.index();
        let was_running = self.tasks[cur].state == State::Running
            && (0..HARTS).all(|h| h == hart || self.hart_current[h] != Some(cur_slot));
        if was_running {
            self.tasks[cur].state = State::Ready;
        }
        self.inherit();
        let mut best: Option<u8> = None;
        for t in &self.tasks {
            if t.state == State::Ready && t.affinity & mask != 0 {
                best = Some(best.map_or(t.effective_priority, |b| b.max(t.effective_priority)));
            }
        }
        let Some(best_prio) = best else {
            self.hart_current[hart] = None;
            return None;
        };
        if let Some(preferred) = self.preferred {
            let idx = preferred.index();
            let t = &self.tasks[idx];
            if t.state == State::Ready
                && t.affinity & mask != 0
                && t.effective_priority == best_prio
            {
                self.preferred = None;
                if self.tasks[idx].last_hart != hart as u8
                    && (self.tasks[idx].runtime_ticks > 0
                        || self.tasks[idx].user_cycles > 0
                        || self.tasks[idx].preemptions > 0)
                {
                    self.tasks[idx].migrations = self.tasks[idx].migrations.saturating_add(1);
                }
                self.tasks[idx].last_hart = hart as u8;
                self.tasks[idx].state = State::Running;
                self.hart_current[hart] = Some(preferred);
                self.hart_last[hart] = preferred;
                self.current = preferred;
                return Some(preferred);
            }
        }
        for offset in 1..=self.tasks.len() {
            let next = (cur + offset) % self.tasks.len();
            if self.tasks[next].state == State::Ready
                && self.tasks[next].affinity & mask != 0
                && self.tasks[next].effective_priority == best_prio
            {
                if preemptive && was_running && next != cur {
                    self.tasks[cur].preemptions += 1;
                }
                if self.tasks[next].last_hart != hart as u8
                    && (self.tasks[next].runtime_ticks > 0
                        || self.tasks[next].user_cycles > 0
                        || self.tasks[next].preemptions > 0)
                {
                    self.tasks[next].migrations = self.tasks[next].migrations.saturating_add(1);
                }
                self.tasks[next].last_hart = hart as u8;
                self.tasks[next].state = State::Running;
                let slot = ThreadSlot(next);
                self.hart_current[hart] = Some(slot);
                self.hart_last[hart] = slot;
                self.current = slot;
                return Some(slot);
            }
        }
        self.hart_current[hart] = None;
        None
    }
    pub fn prefer(&mut self, thread: ThreadSlot) {
        self.preferred = Some(thread);
    }
    pub fn should_preempt(&self) -> bool {
        self.should_preempt_for(self.active_hart)
    }
    pub fn should_preempt_for(&self, hart: usize) -> bool {
        assert!(hart < HARTS);
        let mask = 1u8 << hart;
        let cur_slot = self.hart_current[hart].unwrap_or(self.current);
        let cur_task = &self.tasks[cur_slot.index()];
        if cur_task.state != State::Running || cur_task.affinity & mask == 0 {
            return self
                .tasks
                .iter()
                .any(|t| t.state == State::Ready && t.affinity & mask != 0);
        }
        self.tasks.iter().any(|t| {
            t.state == State::Ready
                && t.affinity & mask != 0
                && t.effective_priority > cur_task.effective_priority
        })
    }
    /// Return `true` if `hart` is currently idle while a compatible `Ready` thread
    /// exists, or if a higher-priority `Ready` thread can preempt `hart` (or round-robin
    /// on a timer tick when `timer_tick` is true).
    pub fn needs_reschedule_on(&self, hart: usize, timer_tick: bool) -> bool {
        assert!(hart < HARTS);
        let mask = 1u8 << hart;
        let Some(cur_slot) = self.hart_current[hart] else {
            return self
                .tasks
                .iter()
                .any(|t| t.state == State::Ready && t.affinity & mask != 0);
        };
        let cur_task = &self.tasks[cur_slot.index()];
        if cur_task.state != State::Running || cur_task.affinity & mask == 0 {
            return true;
        }
        self.tasks.iter().any(|t| {
            t.state == State::Ready
                && t.affinity & mask != 0
                && (t.effective_priority > cur_task.effective_priority
                    || (timer_tick && t.effective_priority == cur_task.effective_priority))
        })
    }
    pub fn inherit(&mut self) {
        let mut any_reply = false;
        for t in &mut self.tasks {
            t.effective_priority = t.priority;
            if t.state == State::Blocked && matches!(t.wait, Wait::Reply { .. }) {
                any_reply = true;
            }
        }
        if !any_reply {
            return;
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
        self.block_opt(wait, None);
    }
    pub fn block_until(&mut self, wait: Wait, deadline: Ticks) {
        self.block_opt(wait, Some(deadline));
    }
    pub fn block_opt(&mut self, wait: Wait, deadline: Option<Ticks>) {
        let cur = self.current.index();
        self.tasks[cur].wait = wait;
        self.tasks[cur].deadline = deadline;
        self.tasks[cur].state = State::Blocked;
        for slot in &mut self.hart_current {
            if *slot == Some(ThreadSlot(cur)) {
                *slot = None;
            }
        }
    }
    pub fn wake(&mut self, task: ThreadSlot) {
        let idx = task.index();
        if self.tasks[idx].state == State::Blocked {
            self.tasks[idx].wait = Wait::None;
            self.tasks[idx].deadline = None;
            self.tasks[idx].state = State::Ready;
        }
    }
    pub fn terminate(&mut self) {
        let cur = self.current.index();
        self.tasks[cur].wait = Wait::None;
        self.tasks[cur].deadline = None;
        self.tasks[cur].state = State::Terminated;
        for slot in &mut self.hart_current {
            if *slot == Some(ThreadSlot(cur)) {
                *slot = None;
            }
        }
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

impl<C: Context> Scheduler<C> {
    /// Store `a0` as the blocked task's syscall result, then make it runnable.
    pub fn wake_with(&mut self, task: ThreadSlot, a0: u32) {
        self.task_mut(task).context.set_return(a0);
        self.wake(task);
    }
    /// Like [`Self::wake_with`] with a second result register.
    pub fn wake_with2(&mut self, task: ThreadSlot, a0: u32, a1: u32) {
        self.task_mut(task).context.set_return(a0);
        self.task_mut(task).context.set_return2(a1);
        self.wake(task);
    }
    /// Advance kernel time, expire all elapsed thread deadlines in a single
    /// O(TASKS) pass, and update 5-second load samples.
    pub fn tick(&mut self, uptime: Ticks, calls: &mut CallTable) {
        self.ticks += 1;
        self.advance_uptime(uptime, calls);
    }

    /// Synchronize `uptime` after waking from a tickless idle window without
    /// counting a synthetic SYSTIMER interrupt if woken early by an external IRQ.
    pub fn sync_uptime(&mut self, uptime: Ticks, calls: &mut CallTable) {
        if uptime > self.uptime {
            self.advance_uptime(uptime, calls);
        }
    }

    fn advance_uptime(&mut self, uptime: Ticks, calls: &mut CallTable) {
        let delta = uptime.saturating_sub(self.uptime);
        let mut accounted_any = false;
        for slot in self.hart_current.into_iter().flatten() {
            if self.tasks[slot.index()].state == State::Running {
                self.tasks[slot.index()].runtime_ticks += delta;
                accounted_any = true;
            }
        }
        if !accounted_any {
            let cur = self.current.index();
            if self.tasks[cur].state == State::Running {
                self.tasks[cur].runtime_ticks += delta;
            }
        }
        for i in 0..TASKS {
            let slot = ThreadSlot(i);
            if self.tasks[i].state != State::Blocked {
                continue;
            }
            if !self.tasks[i].deadline.is_some_and(|d| d <= uptime) {
                continue;
            }
            match self.tasks[i].wait {
                Wait::Sleep => self.wake(slot),
                Wait::Reply { .. } => {
                    calls.deactivate(slot);
                    self.wake_with(slot, Errno::Etimedout.to_abi());
                }
                _ => self.wake_with(slot, Errno::Etimedout.to_abi()),
            }
        }
        self.uptime = uptime;
        let windows = self.uptime.saturating_sub(self.sampled_at) / (5 * HZ);
        if windows > 0 {
            let runnable = self.snapshot().runnable_tasks;
            // Q16 EWMA reaches its integer fixed point well within 2048 windows;
            // bounding the loop keeps the masked-IRQ timer path strictly O(1).
            for _ in 0..windows.min(2048) {
                self.load.sample(runnable);
            }
            self.sampled_at = self
                .sampled_at
                .saturating_add(windows.saturating_mul(5 * HZ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    struct TestCtx {
        a0: u32,
        a1: u32,
    }
    impl Context for TestCtx {
        fn set_return(&mut self, value: u32) {
            self.a0 = value;
        }
        fn set_return2(&mut self, value: u32) {
            self.a1 = value;
        }
    }

    fn test_scheduler() -> Scheduler<TestCtx> {
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
        Scheduler::new([TestCtx::default(); 3], [0x100, 0x200, 0x300], stacks)
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
        });
        s.current = ThreadSlot(2);
        s.tasks[2].state = State::Running;
        s.block(Wait::Reply {
            token: 2,
            pointer: 0,
            capacity: 0,
            server: ThreadSlot(1),
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
    fn tick_wakes_sleepers_expires_device_and_reply_waits_and_samples_load() {
        let mut s = test_scheduler();
        let mut calls = CallTable::new();

        // Thread 0: Sleep until 10 (keeps a0 = 0).
        s.block_until(Wait::Sleep, Ticks(10));
        assert_eq!(s.select(false), Some(ThreadSlot(1)));

        // Thread 1: Device wait until 15 (wakes with ETIMEDOUT).
        s.block_until(
            Wait::Device {
                domain: 2,
                token: 42,
                pointer: 0,
            },
            Ticks(15),
        );
        assert_eq!(s.select(false), Some(ThreadSlot(2)));

        // Thread 2: Reply wait until 20 (deactivates CallTable + ETIMEDOUT).
        let (generation, token) = calls.next_token(ThreadSlot(2)).unwrap();
        calls.activate(ThreadSlot(2), generation, 0);
        calls.bind_reply(&mut s, token, ThreadSlot(0));
        s.block_until(
            Wait::Reply {
                token,
                pointer: 0,
                capacity: 8,
                server: ThreadSlot(0),
            },
            Ticks(20),
        );

        assert!(!s.has_runnable());
        assert_eq!(s.nearest_deadline(), Some(Ticks(10)));
        assert_eq!(s.next_load_sample_at(), Ticks(5 * HZ));

        s.tick(Ticks(5), &mut calls);
        assert_eq!(s.tasks[0].state, State::Blocked);
        assert_eq!(s.tasks[1].state, State::Blocked);
        assert_eq!(s.tasks[2].state, State::Blocked);

        s.tick(Ticks(10), &mut calls);
        assert!(s.has_runnable());
        assert_eq!(s.nearest_deadline(), Some(Ticks(15)));
        assert_eq!(s.tasks[0].state, State::Ready);
        assert_eq!(s.tasks[0].deadline, None);
        assert_eq!(s.tasks[0].context.a0, 0);
        assert_eq!(s.tasks[1].state, State::Blocked);

        s.tick(Ticks(15), &mut calls);
        assert_eq!(s.tasks[1].state, State::Ready);
        assert_eq!(s.tasks[1].context.a0, Errno::Etimedout.to_abi());
        assert_eq!(s.tasks[2].state, State::Blocked);

        s.tick(Ticks(20), &mut calls);
        assert_eq!(s.tasks[2].state, State::Ready);
        assert_eq!(s.tasks[2].context.a0, Errno::Etimedout.to_abi());
        assert_eq!(calls.lookup_for_reply(token), Err(Errno::Epipe));

        // Advance past 5-second sampling window with thread 0 running.
        assert_eq!(s.select(false), Some(ThreadSlot(0)));
        s.tick(Ticks(5 * HZ), &mut calls);
        let snap = s.snapshot();
        assert_eq!(snap.uptime_ticks, 5 * HZ);
        assert!(snap.load_q16[0] > 0);
        assert_eq!(snap.runnable_tasks, 3);
    }

    #[test]
    fn empty_scheduler_populates_and_activates_initial_boot_thread() {
        let mut s = Scheduler::empty(TestCtx::default());
        assert_eq!(s.snapshot().total_tasks, 0);
        let stack = Region {
            base: 0x1000,
            size: 4096,
        };
        let t1 = s
            .create_thread(ProcSlot(0), TestCtx::default(), stack, 0x100, 0, 1)
            .unwrap();
        let t2 = s
            .create_thread(ProcSlot(1), TestCtx::default(), stack, 0x200, 1, 1)
            .unwrap();
        assert_eq!((t1, t2), (Tid(1), Tid(2)));
        s.activate_initial(ThreadSlot(0));
        assert_eq!(s.current, ThreadSlot(0));
        assert_eq!(s.snapshot().running_tasks, 1);
        assert_eq!(s.select(false), Some(ThreadSlot(1)));
    }

    #[test]
    fn create_thread_validates_args_and_fills_terminated_slots() {
        let mut s = test_scheduler();
        let stack = Region {
            base: 0x4000,
            size: 4096,
        };
        assert_eq!(
            s.create_thread(ProcSlot(0), TestCtx::default(), stack, 0x400, 4, 1),
            Err(Errno::Einval)
        );
        assert_eq!(
            s.create_thread(ProcSlot(0), TestCtx::default(), stack, 0x400, 1, 0),
            Err(Errno::Einval)
        );
        assert_eq!(
            s.create_thread(ProcSlot(0), TestCtx::default(), stack, 0x400, 1, 4),
            Err(Errno::Einval)
        );

        for i in 0..(TASKS - BOOT_THREADS) {
            let tid = s
                .create_thread(ProcSlot(0), TestCtx::default(), stack, 0x400, 1, 1)
                .unwrap();
            assert_eq!(tid, Tid(BOOT_THREADS as u32 + i as u32 + 1));
        }
        assert_eq!(
            s.create_thread(ProcSlot(0), TestCtx::default(), stack, 0x400, 1, 1),
            Err(Errno::Enospc)
        );
    }

    #[test]
    fn smp_multi_hart_selection_prevents_concurrent_execution_and_tracks_migration() {
        let mut s = test_scheduler();
        // Make thread 1 eligible for any hart (3 = HP0 | HP1) and thread 2 pinned to HP1 (2).
        s.tasks[1].affinity = 3;
        s.tasks[2].affinity = 2;

        // Thread 0 is Running on HP0; HP1 selects highest-priority eligible thread (1 or 2).
        assert_eq!(s.hart_current[0], Some(ThreadSlot(0)));
        assert!(s.needs_reschedule_on(1, false));
        assert_eq!(s.select_for(1, false), Some(ThreadSlot(1)));
        assert_eq!(s.running_hart(ThreadSlot(0)), Some(0));
        assert_eq!(s.running_hart(ThreadSlot(1)), Some(1));
        assert_eq!(s.snapshot().running_tasks, 2);

        // While thread 1 is Running on HP1, HP0 cannot select thread 1 or HP1-pinned thread 2!
        assert_eq!(s.select_for(0, true), Some(ThreadSlot(0)));

        // Advance uptime: both Running threads (0 on HP0 and 1 on HP1) accumulate runtime_ticks.
        let mut calls = CallTable::new();
        s.tick(Ticks(4), &mut calls);
        assert_eq!(s.tasks[0].runtime_ticks, 4);
        assert_eq!(s.tasks[1].runtime_ticks, 4);

        // Now HP1 switches to thread 2 (making thread 1 Ready), and HP0 blocks thread 0.
        assert_eq!(s.select_for(1, false), Some(ThreadSlot(2)));
        assert_eq!(s.tasks[1].state, State::Ready);
        s.enter_hart(0);
        s.block(Wait::Sleep);
        // HP0 now picks thread 1 (migrating it from HP1 -> HP0!).
        assert_eq!(s.select_for(0, false), Some(ThreadSlot(1)));
        assert_eq!(s.tasks[1].last_hart, 0);
        assert_eq!(s.tasks[1].migrations, 1);
    }
}
