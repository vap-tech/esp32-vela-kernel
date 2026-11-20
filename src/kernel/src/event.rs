//! Generational kernel IRQ / Event slot table.
//!
//! Hardware interrupt lines and peripheral interrupt controllers live in the
//! platform crate; this module owns the pure `no_std` state machine for:
//! - static slot registration (`register`),
//! - process-owned subscription (`bind` / `unbind`) with monotonic generations,
//! - interrupt-handler notification (`notify`), which masks the source and either
//!   wakes a blocked `Wait::Event` waiter or latches `pending = true`,
//! - acknowledgment / re-arming (`ack`), and
//! - wait registration (`wait` with optional `deadline`), including immediate
//!   completion when an interrupt arrived before `EVENT_WAIT`.
//!
//! Revocation (`CAP_REVOKE`) and owner process exit (`remove_process`) cleanly
//! wake blocked waiters with `-EPIPE` and report whether the hardware line must
//! be masked by the platform.

use crate::{
    capability::Object,
    errno::Errno,
    id::{ProcSlot, ThreadSlot, Ticks},
    sched::{Scheduler, TASKS},
    task::{Context, State, Wait},
};

pub const MAX_EVENTS: usize = 8;

/// Outcome of [`EventTable::wait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// A latched pending interrupt was consumed immediately without blocking.
    Immediate,
    /// `check_ready` reported data already available at the peripheral after
    /// re-arming; the caller must not block (and the callback already re-masked
    /// if appropriate).
    ReadyNow,
    /// No pending interrupt and the peripheral was re-armed; the calling thread
    /// is now blocked in `Wait::Event` (with optional `deadline`).
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EventSlot {
    registered: bool,
    line: u32,
    owner: Option<ProcSlot>,
    generation: u32,
    masked: bool,
    pending: bool,
    interrupts: u64,
}

impl EventSlot {
    const EMPTY: Self = Self {
        registered: false,
        line: 0,
        owner: None,
        generation: 0,
        masked: true,
        pending: false,
        interrupts: 0,
    };
}

pub struct EventTable {
    slots: [EventSlot; MAX_EVENTS],
    generation: u32,
}

impl Default for EventTable {
    fn default() -> Self {
        Self::new()
    }
}

impl EventTable {
    pub const fn new() -> Self {
        Self {
            slots: [EventSlot::EMPTY; MAX_EVENTS],
            generation: 0,
        }
    }

    /// Register hardware CLIC `line` at static `index` (`0..MAX_EVENTS`).
    /// The line starts unbound and masked.
    pub fn register(&mut self, index: usize, line: u32) -> Result<(), Errno> {
        if line == 0 || index >= MAX_EVENTS {
            return Err(Errno::Einval);
        }
        if self.slots[index].registered || self.slots.iter().any(|s| s.registered && s.line == line)
        {
            return Err(Errno::Ebusy);
        }
        self.slots[index] = EventSlot {
            registered: true,
            line,
            owner: None,
            generation: 0,
            masked: true,
            pending: false,
            interrupts: 0,
        };
        Ok(())
    }

    /// Look up a registered slot index by its hardware CLIC interrupt `line`.
    pub fn index_by_line(&self, line: u32) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.registered && s.line == line)
    }

    /// Resolve a live bound `Object::Event { index, generation }` to its slot index.
    pub fn resolve(&self, object: Object) -> Result<usize, Errno> {
        let Object::Event { index, generation } = object else {
            return Err(Errno::Ebadf);
        };
        let slot = self.slots.get(index).ok_or(Errno::Ebadf)?;
        if !slot.registered || slot.owner.is_none() || slot.generation != generation {
            return Err(Errno::Epipe);
        }
        Ok(index)
    }

    /// Return the current `Object::Event` identity of `index`.
    pub fn object(&self, index: usize) -> Object {
        Object::Event {
            index,
            generation: self.slots[index].generation,
        }
    }

    pub fn owner(&self, index: usize) -> Option<ProcSlot> {
        self.slots.get(index)?.owner
    }

    pub fn line(&self, index: usize) -> Option<u32> {
        let slot = self.slots.get(index)?;
        slot.registered.then_some(slot.line)
    }

    pub fn is_masked(&self, index: usize) -> bool {
        self.slots.get(index).is_none_or(|s| s.masked)
    }

    pub fn is_pending(&self, index: usize) -> bool {
        self.slots.get(index).is_some_and(|s| s.pending)
    }

    pub fn interrupts(&self, index: usize) -> u64 {
        self.slots.get(index).map_or(0, |s| s.interrupts)
    }

    /// Bind registered slot `index` to `owner` with a fresh generation.
    /// The slot begins masked with `pending = false`.
    pub fn bind(&mut self, index: usize, owner: ProcSlot) -> Result<Object, Errno> {
        let slot = self.slots.get(index).ok_or(Errno::Einval)?;
        if !slot.registered {
            return Err(Errno::Enoent);
        }
        if slot.owner.is_some() {
            return Err(Errno::Ebusy);
        }
        self.generation = self.generation.checked_add(1).ok_or(Errno::Enospc)?;
        let slot = &mut self.slots[index];
        slot.owner = Some(owner);
        slot.generation = self.generation;
        slot.masked = true;
        slot.pending = false;
        Ok(Object::Event {
            index,
            generation: self.generation,
        })
    }

    /// Unbind `index`, masking the slot and waking any blocked waiter with `-EPIPE`.
    pub fn unbind<C: Context>(&mut self, index: usize, scheduler: &mut Scheduler<C>) -> Object {
        let object = self.object(index);
        if let Some(slot) = self.slots.get_mut(index) {
            slot.owner = None;
            slot.masked = true;
            slot.pending = false;
        }
        self.fail_waiters(object, scheduler, None, Errno::Epipe);
        object
    }

    /// Unbind all slots owned by `process`. Returns a bitmask of slot indices
    /// that were bound to `process` so the platform can mask their hardware lines.
    pub fn remove_process<C: Context>(
        &mut self,
        process: ProcSlot,
        scheduler: &mut Scheduler<C>,
    ) -> [Option<Object>; MAX_EVENTS] {
        let mut removed = [None; MAX_EVENTS];
        for (index, item) in removed.iter_mut().enumerate() {
            if self.slots[index].registered && self.slots[index].owner == Some(process) {
                *item = Some(self.unbind(index, scheduler));
            }
        }
        removed
    }

    /// Wake any thread blocked on `object` (excluding `keep_owner` if provided)
    /// with `errno`, and return `true` if no waiter remains on `object`.
    pub fn fail_waiters<C: Context>(
        &mut self,
        object: Object,
        scheduler: &mut Scheduler<C>,
        keep_owner: Option<ProcSlot>,
        errno: Errno,
    ) -> bool {
        let mut any_remaining = false;
        for i in 0..TASKS {
            let thread = ThreadSlot(i);
            let t = scheduler.task(thread);
            if t.state == State::Blocked && t.wait == (Wait::Event { object }) {
                if Some(t.process) == keep_owner {
                    any_remaining = true;
                } else {
                    scheduler.wake_with(thread, errno.to_abi());
                }
            }
        }
        if !any_remaining
            && let Object::Event { index, generation } = object
            && let Some(slot) = self.slots.get_mut(index)
            && slot.registered
            && slot.generation == generation
        {
            slot.masked = true;
            slot.pending = false;
            return true;
        }
        false
    }

    /// Record a hardware IRQ arrival on `index`, mark the source masked, and
    /// wake a blocked `Wait::Event` thread if one exists; otherwise latch
    /// `pending = true` (if the slot is currently bound).
    pub fn notify<C: Context>(
        &mut self,
        index: usize,
        scheduler: &mut Scheduler<C>,
    ) -> Option<ThreadSlot> {
        let slot = self.slots.get_mut(index)?;
        if !slot.registered {
            return None;
        }
        slot.interrupts = slot.interrupts.saturating_add(1);
        slot.masked = true;
        slot.owner?;
        let object = Object::Event {
            index,
            generation: slot.generation,
        };
        for i in 0..TASKS {
            let thread = ThreadSlot(i);
            let t = scheduler.task(thread);
            if t.state == State::Blocked && t.wait == (Wait::Event { object }) {
                slot.pending = false;
                scheduler.wake_with(thread, 0);
                return Some(thread);
            }
        }
        slot.pending = true;
        None
    }

    /// Acknowledge and re-arm `object`. Clears any latched `pending` notification
    /// and marks the slot unmasked (`masked = false`) before invoking `arm_hw`.
    pub fn ack(&mut self, object: Object, arm_hw: impl FnOnce(usize)) -> Result<(), Errno> {
        let index = self.resolve(object)?;
        self.slots[index].pending = false;
        self.slots[index].masked = false;
        arm_hw(index);
        Ok(())
    }

    /// Wait for an interrupt on `object` (with an optional `deadline`).
    ///
    /// - Rejects concurrent waiters on the same event with `Errno::Ebusy`.
    /// - If `pending` was already latched while masked, consumes `pending` and
    ///   returns [`WaitOutcome::Immediate`] (leaving the line masked for the driver).
    /// - Otherwise invokes `arm_and_ keep_waiting(index)`:
    ///   - if it returns `false` (peripheral already has data ready and re-masked
    ///     itself), leaves the slot masked and returns [`WaitOutcome::ReadyNow`];
    ///   - if it returns `true`, marks the slot unmasked and blocks `scheduler.current`
    ///     in `Wait::Event { object }` with `deadline`, returning [`WaitOutcome::Blocked`].
    pub fn wait<C: Copy>(
        &mut self,
        object: Object,
        scheduler: &mut Scheduler<C>,
        deadline: Option<Ticks>,
        arm_and_keep_waiting: impl FnOnce(usize) -> bool,
    ) -> Result<WaitOutcome, Errno> {
        let index = self.resolve(object)?;
        if scheduler
            .tasks
            .iter()
            .any(|t| t.state == State::Blocked && t.wait == (Wait::Event { object }))
        {
            return Err(Errno::Ebusy);
        }
        if self.slots[index].pending {
            self.slots[index].pending = false;
            return Ok(WaitOutcome::Immediate);
        }
        if !arm_and_keep_waiting(index) {
            self.slots[index].masked = true;
            return Ok(WaitOutcome::ReadyNow);
        }
        self.slots[index].masked = false;
        scheduler.block_opt(Wait::Event { object }, deadline);
        Ok(WaitOutcome::Blocked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{call::CallTable, task::Region};

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
    fn register_bind_unbind_enforce_uniqueness_and_generations() {
        let mut events = EventTable::new();
        let mut sched = test_scheduler();

        assert_eq!(events.register(0, 0), Err(Errno::Einval));
        assert_eq!(events.register(MAX_EVENTS, 18), Err(Errno::Einval));
        events.register(0, 18).unwrap();
        // Duplicate slot or duplicate line is rejected.
        assert_eq!(events.register(0, 19), Err(Errno::Ebusy));
        assert_eq!(events.register(1, 18), Err(Errno::Ebusy));
        assert_eq!(events.index_by_line(18), Some(0));
        assert_eq!(events.index_by_line(99), None);

        // Unregistered slot cannot be bound.
        assert_eq!(events.bind(1, ProcSlot(0)), Err(Errno::Enoent));

        let ev1 = events.bind(0, ProcSlot(0)).unwrap();
        assert_eq!(events.resolve(ev1), Ok(0));
        assert_eq!(events.bind(0, ProcSlot(1)), Err(Errno::Ebusy));

        // Unbinding invalidates the previous generation and allows rebinding with a new generation.
        events.unbind(0, &mut sched);
        assert_eq!(events.resolve(ev1), Err(Errno::Epipe));

        let ev2 = events.bind(0, ProcSlot(1)).unwrap();
        assert_ne!(ev1, ev2);
        assert_eq!(events.resolve(ev1), Err(Errno::Epipe));
        assert_eq!(events.resolve(ev2), Ok(0));
    }

    #[test]
    fn wait_blocks_wakes_on_notify_latches_early_irq_and_supports_timeout_and_ack() {
        let mut events = EventTable::new();
        let mut sched = test_scheduler();
        let mut calls = CallTable::new();

        events.register(0, 18).unwrap();
        let ev = events.bind(0, ProcSlot(0)).unwrap();

        // 1. Peripheral already ready during arm callback -> ReadyNow, thread stays Running, slot masked.
        assert_eq!(
            events.wait(ev, &mut sched, None, |_| false),
            Ok(WaitOutcome::ReadyNow)
        );
        assert_eq!(sched.current().state, State::Running);
        assert!(events.is_masked(0));

        // 2. Normal wait blocks thread and unmasks slot.
        assert_eq!(
            events.wait(ev, &mut sched, Some(Ticks(50)), |_| true),
            Ok(WaitOutcome::Blocked)
        );
        assert!(!events.is_masked(0));
        assert_eq!(sched.task(ThreadSlot(0)).state, State::Blocked);
        assert_eq!(sched.task(ThreadSlot(0)).deadline, Some(Ticks(50)));

        // Second concurrent waiter on the same event is rejected with EBUSY.
        sched.select(false).unwrap();
        assert_eq!(
            events.wait(ev, &mut sched, None, |_| true),
            Err(Errno::Ebusy)
        );

        // 3. Hardware IRQ arrives before deadline -> wakes waiter with a0=0 and masks slot.
        assert_eq!(events.notify(0, &mut sched), Some(ThreadSlot(0)));
        assert!(events.is_masked(0));
        assert!(!events.is_pending(0));
        assert_eq!(events.interrupts(0), 1);
        assert_eq!(sched.task(ThreadSlot(0)).state, State::Ready);
        assert_eq!(sched.task(ThreadSlot(0)).context.a0, 0);

        // 4. Hardware IRQ arrives while no thread is waiting -> latches pending=true.
        assert_eq!(events.notify(0, &mut sched), None);
        assert!(events.is_pending(0));
        assert_eq!(events.interrupts(0), 2);

        // Next EVENT_WAIT consumes latched pending immediately without calling arm callback.
        sched.current = ThreadSlot(0);
        sched.task_mut(ThreadSlot(0)).state = State::Running;
        let mut arm_called = false;
        assert_eq!(
            events.wait(ev, &mut sched, None, |_| {
                arm_called = true;
                true
            }),
            Ok(WaitOutcome::Immediate)
        );
        assert!(!arm_called);
        assert!(!events.is_pending(0));

        // 5. Explicit IRQ_ACK clears pending and unmasks.
        assert_eq!(events.notify(0, &mut sched), None);
        assert!(events.is_pending(0));
        let mut acked_slot = None;
        events
            .ack(ev, |idx| {
                acked_slot = Some(idx);
            })
            .unwrap();
        assert_eq!(acked_slot, Some(0));
        assert!(!events.is_pending(0));
        assert!(!events.is_masked(0));

        // 6. Wait with deadline expires via Scheduler::tick with -ETIMEDOUT.
        assert_eq!(
            events.wait(ev, &mut sched, Some(Ticks(10)), |_| true),
            Ok(WaitOutcome::Blocked)
        );
        sched.select(false).unwrap();
        sched.tick(Ticks(10), &mut calls);
        assert_eq!(sched.task(ThreadSlot(0)).state, State::Ready);
        assert_eq!(
            sched.task(ThreadSlot(0)).context.a0,
            Errno::Etimedout.to_abi()
        );
    }
}
