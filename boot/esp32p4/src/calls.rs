use vela_kernel::{
    errno::Errno,
    id::{ThreadSlot, Ticks},
    sched::{Scheduler, TASKS},
    task::{Thread, Wait},
};

use crate::arch;

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

impl CallTable {
    pub const fn new() -> Self {
        Self {
            entries: [Call::EMPTY; TASKS],
        }
    }

    pub fn next_token(&self, client: ThreadSlot) -> Result<(u32, u32), Errno> {
        let current = self.entries[client.index()].generation;
        if current == 0x7f_ffff {
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

    pub fn lookup_for_reply(&self, token: u32) -> Result<(ThreadSlot, ThreadSlot), Errno> {
        let client_idx = (token & 255).wrapping_sub(1) as usize;
        let call = self.entries.get(client_idx).ok_or(Errno::Ebadf)?;
        if !call.active || call.generation != token >> 8 {
            return Err(Errno::Epipe);
        }
        let server = call.server.ok_or(Errno::Eperm)?;
        Ok((ThreadSlot(client_idx), server))
    }

    pub fn bind_reply(
        &mut self,
        scheduler: &mut Scheduler<arch::TrapFrame>,
        token: u32,
        server: ThreadSlot,
    ) {
        if token == 0 {
            return;
        }
        let client_idx = (token & 255).wrapping_sub(1) as usize;
        if let Some(call) = self.entries.get_mut(client_idx) {
            if call.active && call.generation == token >> 8 {
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
        }
    }

    fn fail_one(
        &mut self,
        scheduler: &mut Scheduler<arch::TrapFrame>,
        client: ThreadSlot,
        error: Errno,
    ) {
        self.entries[client.index()].active = false;
        if matches!(scheduler.task(client).wait, Wait::Reply { .. }) {
            scheduler.task_mut(client).context.registers[8] = error.to_abi();
            scheduler.wake(client);
        }
    }

    pub fn fail_where(
        &mut self,
        scheduler: &mut Scheduler<arch::TrapFrame>,
        error: Errno,
        mut predicate: impl FnMut(&Call, &Thread<arch::TrapFrame>) -> bool,
    ) {
        for i in 0..TASKS {
            let client = ThreadSlot(i);
            if self.entries[i].active && predicate(&self.entries[i], scheduler.task(client)) {
                self.fail_one(scheduler, client, error);
            }
        }
    }

    pub fn fail_for_server(
        &mut self,
        scheduler: &mut Scheduler<arch::TrapFrame>,
        server: ThreadSlot,
        error: Errno,
    ) {
        self.fail_where(scheduler, error, |call, _| call.server == Some(server));
    }

    pub fn fail_for_endpoint(
        &mut self,
        scheduler: &mut Scheduler<arch::TrapFrame>,
        endpoint: usize,
        error: Errno,
    ) {
        self.fail_where(scheduler, error, |call, _| call.endpoint == endpoint);
    }

    pub fn expire(&mut self, scheduler: &mut Scheduler<arch::TrapFrame>, uptime: Ticks) {
        self.fail_where(scheduler, Errno::Etimedout, |_, task| {
            matches!(
                task.wait,
                Wait::Reply {
                    until: Some(deadline),
                    ..
                } if deadline <= uptime
            )
        });
    }
}
