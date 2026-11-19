//! RPC call table: one outstanding call per client thread, tracked by a
//! generation-stamped reply token (`generation << 8 | client_slot + 1`).
//! Architecture-independent; the saved-register layout is reached only through
//! [`Context`].

use crate::{
    errno::Errno,
    id::{ThreadSlot, Ticks},
    sched::{Scheduler, TASKS},
    task::{Context, Thread, Wait},
};

pub const MAX_GENERATION: u32 = 0x7f_ffff;

#[derive(Clone, Copy)]
pub struct Call {
    generation: u32,
    active: bool,
    endpoint: usize,
    server: Option<ThreadSlot>,
}

impl Call {
    const EMPTY: Self = Self {
        generation: 0,
        active: false,
        endpoint: 0,
        server: None,
    };

    pub fn endpoint(&self) -> usize {
        self.endpoint
    }
}

pub struct CallTable {
    entries: [Call; TASKS],
}

impl Default for CallTable {
    fn default() -> Self {
        Self::new()
    }
}

impl CallTable {
    pub const fn new() -> Self {
        Self {
            entries: [Call::EMPTY; TASKS],
        }
    }

    /// Next `(generation, reply_token)` for `client`; never reuses a generation.
    pub fn next_token(&self, client: ThreadSlot) -> Result<(u32, u32), Errno> {
        let current = self.entries[client.index()].generation;
        if current == MAX_GENERATION {
            return Err(Errno::Enospc);
        }
        let generation = current + 1;
        let token = (generation << 8) | (client.index() as u32 + 1);
        Ok((generation, token))
    }

    pub fn activate(&mut self, client: ThreadSlot, generation: u32, endpoint: usize) {
        self.entries[client.index()] = Call {
            generation,
            active: true,
            endpoint,
            server: None,
        };
    }

    pub fn deactivate(&mut self, client: ThreadSlot) {
        self.entries[client.index()].active = false;
    }

    /// Resolve a reply token to `(client, bound_server)`.
    pub fn lookup_for_reply(&self, token: u32) -> Result<(ThreadSlot, ThreadSlot), Errno> {
        let client_idx = (token & 255).wrapping_sub(1) as usize;
        let call = self.entries.get(client_idx).ok_or(Errno::Ebadf)?;
        if !call.active || call.generation != token >> 8 {
            return Err(Errno::Epipe);
        }
        let server = call.server.ok_or(Errno::Eperm)?;
        Ok((ThreadSlot(client_idx), server))
    }

    /// Record which server thread received the request carrying `token`, in
    /// both the table and the client's `Wait::Reply`. Stale tokens are ignored.
    pub fn bind_reply<C: Copy>(
        &mut self,
        scheduler: &mut Scheduler<C>,
        token: u32,
        server: ThreadSlot,
    ) {
        if token == 0 {
            return;
        }
        let client_idx = (token & 255).wrapping_sub(1) as usize;
        let Some(call) = self.entries.get_mut(client_idx) else {
            return;
        };
        if !call.active || call.generation != token >> 8 {
            return;
        }
        let client = ThreadSlot(client_idx);
        call.server = Some(server);
        if let Wait::Reply {
            token,
            pointer,
            capacity,
            until,
            ..
        } = scheduler.task(client).wait
        {
            scheduler.task_mut(client).wait = Wait::Reply {
                token,
                pointer,
                capacity,
                until,
                server,
            };
        }
    }

    /// Fail every active call matching `predicate`, waking blocked clients
    /// with `-error` in their return register.
    pub fn fail_where<C: Context>(
        &mut self,
        scheduler: &mut Scheduler<C>,
        error: Errno,
        mut predicate: impl FnMut(&Call, &Thread<C>) -> bool,
    ) {
        for i in 0..TASKS {
            let client = ThreadSlot(i);
            if !self.entries[i].active || !predicate(&self.entries[i], scheduler.task(client)) {
                continue;
            }
            self.entries[i].active = false;
            if matches!(scheduler.task(client).wait, Wait::Reply { .. }) {
                scheduler.wake_with(client, error.to_abi());
            }
        }
    }

    pub fn fail_for_server<C: Context>(
        &mut self,
        scheduler: &mut Scheduler<C>,
        server: ThreadSlot,
        error: Errno,
    ) {
        self.fail_where(scheduler, error, |call, _| call.server == Some(server));
    }

    pub fn fail_for_endpoint<C: Context>(
        &mut self,
        scheduler: &mut Scheduler<C>,
        endpoint: usize,
        error: Errno,
    ) {
        self.fail_where(scheduler, error, |call, _| call.endpoint == endpoint);
    }

    /// Time out calls whose `Wait::Reply.until` deadline has passed.
    pub fn expire<C: Context>(&mut self, scheduler: &mut Scheduler<C>, uptime: Ticks) {
        self.fail_where(scheduler, Errno::Etimedout, |_, task| {
            matches!(
                task.wait,
                Wait::Reply { until: Some(deadline), .. } if deadline <= uptime
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        id::Ticks,
        task::{Region, State},
    };

    #[derive(Clone, Copy, Default)]
    struct Ctx {
        a0: u32,
        a1: u32,
    }
    impl Context for Ctx {
        fn set_return(&mut self, value: u32) {
            self.a0 = value;
        }
        fn set_return2(&mut self, value: u32) {
            self.a1 = value;
        }
    }

    fn scheduler() -> Scheduler<Ctx> {
        let region = Region { base: 0, size: 0 };
        Scheduler::new([Ctx::default(); 3], [0; 3], [region; 3])
    }

    fn block_reply(s: &mut Scheduler<Ctx>, client: usize, token: u32, until: Option<Ticks>) {
        s.current = ThreadSlot(client);
        s.block(Wait::Reply {
            token,
            pointer: 0x100,
            capacity: 8,
            server: ThreadSlot(2),
            until,
        });
    }

    fn start_call(
        calls: &mut CallTable,
        s: &mut Scheduler<Ctx>,
        client: usize,
        endpoint: usize,
        until: Option<Ticks>,
    ) -> u32 {
        let (generation, token) = calls.next_token(ThreadSlot(client)).unwrap();
        calls.activate(ThreadSlot(client), generation, endpoint);
        block_reply(s, client, token, until);
        token
    }

    #[test]
    fn tokens_encode_slot_and_never_reuse_generation() {
        let mut calls = CallTable::new();
        assert_eq!(calls.next_token(ThreadSlot(1)).unwrap(), (1, (1 << 8) | 2));
        calls.activate(ThreadSlot(1), 1, 0);
        calls.deactivate(ThreadSlot(1));
        // Deactivation keeps the generation: tokens are never recycled.
        assert_eq!(calls.next_token(ThreadSlot(1)).unwrap().0, 2);
        calls.activate(ThreadSlot(1), 2, 0);
        assert_eq!(calls.next_token(ThreadSlot(1)).unwrap().0, 3);
        calls.activate(ThreadSlot(0), MAX_GENERATION, 0);
        assert_eq!(calls.next_token(ThreadSlot(0)), Err(Errno::Enospc));
        // Other clients are unaffected by one client's exhaustion.
        assert!(calls.next_token(ThreadSlot(2)).is_ok());
    }

    #[test]
    fn lookup_rejects_bad_stale_inactive_and_unbound_tokens() {
        let mut calls = CallTable::new();
        let mut s = scheduler();
        assert_eq!(calls.lookup_for_reply(0), Err(Errno::Ebadf));
        // Slot index TASKS (token low byte TASKS + 1) is out of range.
        assert_eq!(calls.lookup_for_reply(TASKS as u32 + 1), Err(Errno::Ebadf));
        let token = start_call(&mut calls, &mut s, 0, 7, None);
        assert_eq!(calls.lookup_for_reply(token), Err(Errno::Eperm));
        calls.bind_reply(&mut s, token, ThreadSlot(2));
        assert_eq!(
            calls.lookup_for_reply(token),
            Ok((ThreadSlot(0), ThreadSlot(2)))
        );
        assert_eq!(calls.lookup_for_reply(token + (1 << 8)), Err(Errno::Epipe));
        calls.deactivate(ThreadSlot(0));
        assert_eq!(calls.lookup_for_reply(token), Err(Errno::Epipe));
    }

    #[test]
    fn bind_updates_client_wait_and_ignores_stale_or_zero_tokens() {
        let mut calls = CallTable::new();
        let mut s = scheduler();
        let token = start_call(&mut calls, &mut s, 1, 0, Some(Ticks(50)));
        calls.bind_reply(&mut s, 0, ThreadSlot(0));
        calls.bind_reply(&mut s, token + (1 << 8), ThreadSlot(0));
        assert!(matches!(
            s.task(ThreadSlot(1)).wait,
            Wait::Reply { server, .. } if server == ThreadSlot(2)
        ));
        calls.bind_reply(&mut s, token, ThreadSlot(0));
        assert!(matches!(
            s.task(ThreadSlot(1)).wait,
            Wait::Reply {
                server,
                until: Some(Ticks(50)),
                capacity: 8,
                pointer: 0x100,
                ..
            } if server == ThreadSlot(0)
        ));
    }

    #[test]
    fn fail_for_server_and_endpoint_wake_matching_clients_only() {
        let mut calls = CallTable::new();
        let mut s = scheduler();
        let t0 = start_call(&mut calls, &mut s, 0, 3, None);
        let t1 = start_call(&mut calls, &mut s, 1, 4, None);
        calls.bind_reply(&mut s, t0, ThreadSlot(2));
        calls.bind_reply(&mut s, t1, ThreadSlot(1));
        calls.fail_for_server(&mut s, ThreadSlot(2), Errno::Epipe);
        assert_eq!(s.task(ThreadSlot(0)).state, State::Ready);
        assert_eq!(s.task(ThreadSlot(0)).context.a0, Errno::Epipe.to_abi());
        assert_eq!(s.task(ThreadSlot(1)).state, State::Blocked);
        calls.fail_for_endpoint(&mut s, 4, Errno::Ecanceled);
        assert_eq!(s.task(ThreadSlot(1)).state, State::Ready);
        assert_eq!(s.task(ThreadSlot(1)).context.a0, Errno::Ecanceled.to_abi());
        assert_eq!(calls.lookup_for_reply(t0), Err(Errno::Epipe));
        assert_eq!(calls.lookup_for_reply(t1), Err(Errno::Epipe));
    }

    #[test]
    fn failing_a_call_whose_client_is_not_waiting_only_deactivates_it() {
        let mut calls = CallTable::new();
        let mut s = scheduler();
        let token = start_call(&mut calls, &mut s, 0, 1, None);
        s.wake(ThreadSlot(0));
        s.task_mut(ThreadSlot(0)).context.a0 = 99;
        calls.fail_where(&mut s, Errno::Epipe, |_, _| true);
        assert_eq!(s.task(ThreadSlot(0)).context.a0, 99);
        assert_eq!(calls.lookup_for_reply(token), Err(Errno::Epipe));
    }

    #[test]
    fn expire_times_out_only_passed_deadlines() {
        let mut calls = CallTable::new();
        let mut s = scheduler();
        start_call(&mut calls, &mut s, 0, 1, Some(Ticks(10)));
        start_call(&mut calls, &mut s, 1, 1, Some(Ticks(20)));
        start_call(&mut calls, &mut s, 2, 1, None);
        calls.expire(&mut s, Ticks(9));
        assert!((0..3).all(|i| s.task(ThreadSlot(i)).state == State::Blocked));
        calls.expire(&mut s, Ticks(10));
        assert_eq!(s.task(ThreadSlot(0)).state, State::Ready);
        assert_eq!(s.task(ThreadSlot(0)).context.a0, Errno::Etimedout.to_abi());
        assert_eq!(s.task(ThreadSlot(1)).state, State::Blocked);
        calls.expire(&mut s, Ticks(u64::MAX));
        assert_eq!(s.task(ThreadSlot(1)).state, State::Ready);
        assert_eq!(s.task(ThreadSlot(2)).state, State::Blocked);
    }
}
