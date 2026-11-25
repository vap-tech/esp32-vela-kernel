//! RPC call table: one outstanding call per client thread, tracked by a
//! generation-stamped reply token (`generation << 8 | client_slot + 1`).
//! Architecture-independent; the saved-register layout is reached only through
//! [`Context`].

use crate::{
    capability::Capability,
    errno::Errno,
    id::{ProcSlot, ThreadSlot},
    sched::{Scheduler, TASKS},
    task::{Context, Thread, Wait},
};

pub const MAX_GENERATION: u32 = 0x7f_ffff;
pub const BULK_READ: u8 = 1;
pub const BULK_WRITE: u8 = 2;

/// Client-owned bulk memory window attached to an in-flight synchronous RPC.
/// Survives transitive forwarding (`client -> proxy -> worker`) and stays valid
/// only while the root client's call slot remains active with `root_gen`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BulkWindow {
    pub root_thread: ThreadSlot,
    pub root_gen: u32,
    pub owner: ProcSlot,
    pub ptr: usize,
    pub len: usize,
    pub flags: u8,
}

/// Optional per-call metadata carried across an active synchronous RPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallExtra {
    pub bulk: Option<BulkWindow>,
    pub caller_rights: u16,
    pub attached_cap: Option<Capability>,
}

impl CallExtra {
    pub const EMPTY: Self = Self {
        bulk: None,
        caller_rights: 0,
        attached_cap: None,
    };
}

#[derive(Clone, Copy)]
pub struct Call {
    generation: u32,
    active: bool,
    endpoint: usize,
    server: Option<ThreadSlot>,
    bulk: Option<BulkWindow>,
    caller_rights: u16,
    attached_cap: Option<Capability>,
}

impl Call {
    const EMPTY: Self = Self {
        generation: 0,
        active: false,
        endpoint: 0,
        server: None,
        bulk: None,
        caller_rights: 0,
        attached_cap: None,
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
        self.activate_extra(client, generation, endpoint, CallExtra::EMPTY);
    }

    pub fn activate_with_bulk(
        &mut self,
        client: ThreadSlot,
        generation: u32,
        endpoint: usize,
        bulk: Option<BulkWindow>,
    ) {
        self.activate_extra(
            client,
            generation,
            endpoint,
            CallExtra {
                bulk,
                ..CallExtra::EMPTY
            },
        );
    }

    pub fn activate_extra(
        &mut self,
        client: ThreadSlot,
        generation: u32,
        endpoint: usize,
        extra: CallExtra,
    ) {
        self.entries[client.index()] = Call {
            generation,
            active: true,
            endpoint,
            server: None,
            bulk: extra.bulk,
            caller_rights: extra.caller_rights,
            attached_cap: extra.attached_cap,
        };
    }

    pub fn deactivate(&mut self, client: ThreadSlot) {
        let entry = &mut self.entries[client.index()];
        entry.active = false;
        entry.bulk = None;
        entry.caller_rights = 0;
        entry.attached_cap = None;
    }

    /// Inspect `(caller_rights, has_attached_cap)` for an active call bound to `server`.
    pub fn query_meta(&self, token: u32, server: ThreadSlot) -> Result<(u16, bool), Errno> {
        let (client, bound_server) = self.lookup_for_reply(token)?;
        if bound_server != server {
            return Err(Errno::Eperm);
        }
        let call = &self.entries[client.index()];
        Ok((call.caller_rights, call.attached_cap.is_some()))
    }

    /// Consume the optional capability attached to `token` held by `server`, returning
    /// `(caller_rights, attached_cap)`.
    pub fn take_attached_cap(
        &mut self,
        token: u32,
        server: ThreadSlot,
    ) -> Result<(u16, Option<Capability>), Errno> {
        let (client, bound_server) = self.lookup_for_reply(token)?;
        if bound_server != server {
            return Err(Errno::Eperm);
        }
        let call = &mut self.entries[client.index()];
        Ok((call.caller_rights, call.attached_cap.take()))
    }

    fn verify_bulk_live(&self, bulk: BulkWindow) -> Result<BulkWindow, Errno> {
        let root = self
            .entries
            .get(bulk.root_thread.index())
            .ok_or(Errno::Epipe)?;
        if !root.active || root.generation != bulk.root_gen {
            return Err(Errno::Epipe);
        }
        Ok(bulk)
    }

    /// Resolve a bulk window attached to `token` held by `server`, verifying that
    /// both `token` and the root client's RPC are still active.
    pub fn lookup_bulk(&self, token: u32, server: ThreadSlot) -> Result<BulkWindow, Errno> {
        let (client, bound_server) = self.lookup_for_reply(token)?;
        if bound_server != server {
            return Err(Errno::Eperm);
        }
        let bulk = self.entries[client.index()].bulk.ok_or(Errno::Einval)?;
        self.verify_bulk_live(bulk)
    }

    /// Validate a bulk read or write at `offset..offset + len` against the bulk
    /// window attached to `token`, returning `(owner_process, target_addr)`.
    pub fn prepare_bulk_io(
        &self,
        token: u32,
        server: ThreadSlot,
        offset: usize,
        len: usize,
        write_to_client: bool,
    ) -> Result<(ProcSlot, usize), Errno> {
        let bulk = self.lookup_bulk(token, server)?;
        let need = if write_to_client {
            BULK_WRITE
        } else {
            BULK_READ
        };
        if bulk.flags & need != need {
            return Err(Errno::Eperm);
        }
        let end = offset.checked_add(len).ok_or(Errno::Einval)?;
        if end > bulk.len {
            return Err(Errno::Einval);
        }
        let addr = bulk.ptr.checked_add(offset).ok_or(Errno::Efault)?;
        Ok((bulk.owner, addr))
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
            ..
        } = scheduler.task(client).wait
        {
            scheduler.task_mut(client).wait = Wait::Reply {
                token,
                pointer,
                capacity,
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
            self.entries[i].bulk = None;
            self.entries[i].caller_rights = 0;
            self.entries[i].attached_cap = None;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        capability::{ADMIN, Object, SEND},
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

    fn start_call(
        calls: &mut CallTable,
        s: &mut Scheduler<Ctx>,
        client: usize,
        endpoint: usize,
        until: Option<Ticks>,
    ) -> u32 {
        let (generation, token) = calls.next_token(ThreadSlot(client)).unwrap();
        calls.activate(ThreadSlot(client), generation, endpoint);
        s.current = ThreadSlot(client);
        s.block_opt(
            Wait::Reply {
                token,
                pointer: 0x100,
                capacity: 8,
                server: ThreadSlot(2),
            },
            until,
        );
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
    fn bind_updates_client_wait_and_preserves_thread_deadline() {
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
                capacity: 8,
                pointer: 0x100,
                ..
            } if server == ThreadSlot(0)
        ));
        assert_eq!(s.task(ThreadSlot(1)).deadline, Some(Ticks(50)));
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
    fn scheduler_tick_expires_only_passed_call_deadlines() {
        let mut calls = CallTable::new();
        let mut s = scheduler();
        let t0 = start_call(&mut calls, &mut s, 0, 1, Some(Ticks(10)));
        let t1 = start_call(&mut calls, &mut s, 1, 1, Some(Ticks(20)));
        start_call(&mut calls, &mut s, 2, 1, None);
        s.tick(Ticks(9), &mut calls);
        assert!((0..3).all(|i| s.task(ThreadSlot(i)).state == State::Blocked));
        s.tick(Ticks(10), &mut calls);
        assert_eq!(s.task(ThreadSlot(0)).state, State::Ready);
        assert_eq!(s.task(ThreadSlot(0)).context.a0, Errno::Etimedout.to_abi());
        assert_eq!(calls.lookup_for_reply(t0), Err(Errno::Epipe));
        assert_eq!(s.task(ThreadSlot(1)).state, State::Blocked);
        s.tick(Ticks(u64::MAX), &mut calls);
        assert_eq!(s.task(ThreadSlot(1)).state, State::Ready);
        assert_eq!(calls.lookup_for_reply(t1), Err(Errno::Epipe));
        assert_eq!(s.task(ThreadSlot(2)).state, State::Blocked);
    }

    #[test]
    fn bulk_window_supports_forwarding_and_invalidates_when_root_expires() {
        let mut calls = CallTable::new();
        let mut s = scheduler();
        // Client (slot 0, proc 0) calls proxy (slot 1) with a writable bulk window.
        let (gen0, tok0) = calls.next_token(ThreadSlot(0)).unwrap();
        let root_bulk = BulkWindow {
            root_thread: ThreadSlot(0),
            root_gen: gen0,
            owner: ProcSlot(0),
            ptr: 0x8800_0000,
            len: 4096,
            flags: BULK_WRITE,
        };
        calls.activate_with_bulk(ThreadSlot(0), gen0, 1, Some(root_bulk));
        calls.bind_reply(&mut s, tok0, ThreadSlot(1));

        // Only the bound server (slot 1) can inspect or forward the bulk window.
        assert_eq!(calls.lookup_bulk(tok0, ThreadSlot(2)), Err(Errno::Eperm));
        let forwarded = calls.lookup_bulk(tok0, ThreadSlot(1)).unwrap();
        assert_eq!(forwarded, root_bulk);

        // Proxy (slot 1) forwards the bulk window to worker (slot 2).
        let (gen1, tok1) = calls.next_token(ThreadSlot(1)).unwrap();
        calls.activate_with_bulk(ThreadSlot(1), gen1, 2, Some(forwarded));
        calls.bind_reply(&mut s, tok1, ThreadSlot(2));

        // Worker writes into root client's memory; reads are rejected by flags.
        assert_eq!(
            calls.prepare_bulk_io(tok1, ThreadSlot(2), 512, 1024, true),
            Ok((ProcSlot(0), 0x8800_0200))
        );
        assert_eq!(
            calls.prepare_bulk_io(tok1, ThreadSlot(2), 0, 16, false),
            Err(Errno::Eperm)
        );
        assert_eq!(
            calls.prepare_bulk_io(tok1, ThreadSlot(2), 4000, 100, true),
            Err(Errno::Einval)
        );

        // If root client's RPC times out or exits, the forwarded window fails with EPIPE.
        calls.deactivate(ThreadSlot(0));
        assert_eq!(
            calls.prepare_bulk_io(tok1, ThreadSlot(2), 0, 16, true),
            Err(Errno::Epipe)
        );
    }

    #[test]
    fn attached_cap_and_caller_rights_are_server_scoped_and_single_use() {
        let mut calls = CallTable::new();
        let mut s = scheduler();
        let (gen0, tok0) = calls.next_token(ThreadSlot(0)).unwrap();
        let cap = Capability {
            object: Object::Endpoint {
                index: 3,
                generation: 7,
            },
            rights: SEND,
        };
        calls.activate_extra(
            ThreadSlot(0),
            gen0,
            1,
            CallExtra {
                bulk: None,
                caller_rights: SEND | ADMIN,
                attached_cap: Some(cap),
            },
        );
        calls.bind_reply(&mut s, tok0, ThreadSlot(1));

        assert_eq!(calls.query_meta(tok0, ThreadSlot(2)), Err(Errno::Eperm));
        assert_eq!(
            calls.query_meta(tok0, ThreadSlot(1)),
            Ok((SEND | ADMIN, true))
        );
        assert_eq!(
            calls.take_attached_cap(tok0, ThreadSlot(1)),
            Ok((SEND | ADMIN, Some(cap)))
        );
        // Single-use: a second take yields None while preserving caller_rights.
        assert_eq!(
            calls.query_meta(tok0, ThreadSlot(1)),
            Ok((SEND | ADMIN, false))
        );
        assert_eq!(
            calls.take_attached_cap(tok0, ThreadSlot(1)),
            Ok((SEND | ADMIN, None))
        );
    }
}
