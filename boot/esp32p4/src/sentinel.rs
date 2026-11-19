use vela_kernel::{
    errno::Errno,
    id::{Pid, ThreadSlot, Ticks},
    process::Processes,
    sched::{Scheduler, TASKS},
    sentinel::{Heartbeat, WakePhase, WakeTicket},
    task::{State, Wait},
};

use crate::{arch, sentinel_wire, soc, usermem::copy_to_user};

pub struct SentinelClient {
    epoch: Option<u32>,
    ready: bool,
    wake: Option<WakeTicket>,
    sequence: u32,
    pending: bool,
    command: u32,
    argument: u32,
    diagnostics: u32,
    hz: u32,
    heartbeat_link: Heartbeat,
    heartbeat: bool,
    next_beat: Ticks,
    notifications: u32,
    last_publish: u32,
}

impl SentinelClient {
    pub fn new() -> Self {
        Self {
            epoch: None,
            ready: false,
            wake: None,
            sequence: 0,
            pending: false,
            command: 0,
            argument: 0,
            diagnostics: 0,
            hz: 0,
            heartbeat_link: Heartbeat::default(),
            heartbeat: true,
            next_beat: Ticks::ZERO,
            notifications: 0,
            last_publish: 0,
        }
    }

    pub fn set_boot_info(&mut self, epoch: u32, hz: u32) {
        self.epoch = Some(epoch);
        self.hz = hz;
    }

    pub fn epoch(&self) -> Option<u32> {
        self.epoch
    }

    pub fn wake_ticket(&self) -> Option<WakeTicket> {
        self.wake
    }

    pub fn set_wake_ticket(&mut self, ticket: Option<WakeTicket>) {
        self.wake = ticket;
    }

    pub fn release_wake_on_exit(&mut self, owner: Pid) {
        if self
            .wake
            .is_some_and(|ticket| ticket.release_on_exit(owner))
        {
            self.wake = None;
        }
    }

    pub fn set_heartbeat(&mut self, enabled: bool) {
        self.heartbeat = enabled;
        if self.heartbeat {
            self.next_beat = Ticks::ZERO;
        }
    }

    pub fn status(&self) -> (u32, Result<u32, Errno>) {
        let epoch = self.epoch.unwrap_or(0);
        let result = if self.heartbeat_link.offline {
            Err(Errno::Etimedout)
        } else if self.ready {
            Ok(sentinel_wire::VERSION)
        } else {
            Ok(0)
        };
        (epoch, result)
    }

    pub fn request(&mut self, command: u32, arg0: u32, arg1: u32) -> Result<u32, Errno> {
        if !self.ready {
            return Err(Errno::Eagain);
        }
        if self.heartbeat_link.offline {
            return Err(Errno::Etimedout);
        }
        if self.pending {
            return Err(Errno::Ebusy);
        }
        if self.sequence == i32::MAX as u32 {
            return Err(Errno::Eoverflow);
        }
        self.sequence += 1;
        self.pending = true;
        self.command = command;
        self.argument = arg0;
        // SAFETY: hart0 MIE=0, only one pending request until its exact response.
        unsafe {
            soc::sentinel::request(self.sequence, command, arg0, arg1);
        }
        Ok(self.sequence)
    }

    pub fn tick(&mut self, uptime: Ticks) {
        if self.heartbeat_link.retry_due(uptime) {
            // Keep the slot and payload stable until the original heartbeat completes.
            unsafe {
                soc::sentinel::retry_heartbeat(self.sequence);
            }
            return;
        }
        if self.ready
            && self.heartbeat
            && !self
                .wake
                .is_some_and(|t| t.sleep && !matches!(t.phase, WakePhase::Fired(_)))
            && !self.pending
            && uptime >= self.next_beat
            && self.request(sentinel_wire::HEARTBEAT, 3000, 0).is_ok()
        {
            self.heartbeat_link.sent(self.sequence, uptime);
            self.next_beat = uptime.saturating_add(100);
        }
    }

    pub fn expire_waiters(&self, scheduler: &mut Scheduler<arch::TrapFrame>, uptime: Ticks) {
        if self.pending {
            for i in 0..TASKS {
                let thread = ThreadSlot(i);
                if matches!(
                    scheduler.task(thread).wait,
                    Wait::Sentinel { deadline, .. } if uptime >= deadline
                ) {
                    scheduler.task_mut(thread).context.registers[8] = Errno::Etimedout.to_abi();
                    scheduler.wake(thread);
                }
            }
        }
        for i in 0..TASKS {
            let thread = ThreadSlot(i);
            if scheduler.task(thread).state == State::Blocked
                && matches!(
                    scheduler.task(thread).wait,
                    Wait::SentinelWake { deadline, .. } if uptime >= deadline
                )
            {
                scheduler.task_mut(thread).context.registers[8] = Errno::Etimedout.to_abi();
                scheduler.task_mut(thread).context.registers[9] = 0;
                scheduler.wake(thread);
                // Keep the LP token reserved: a late event cannot complete a new alarm.
            }
        }
    }

    fn finish_wake_waiter(
        scheduler: &mut Scheduler<arch::TrapFrame>,
        processes: &Processes,
        ticket: WakeTicket,
        result: u32,
        elapsed: u32,
    ) -> bool {
        if let Some(i) = scheduler.tasks.iter().position(|t| {
            t.state == State::Blocked
                && processes.slot(t.process).id == ticket.owner
                && matches!(t.wait, Wait::SentinelWake { token, .. } if token == ticket.token)
        }) {
            let thread = ThreadSlot(i);
            scheduler.task_mut(thread).context.registers[8] = result;
            scheduler.task_mut(thread).context.registers[9] = elapsed;
            scheduler.wake(thread);
            return true;
        }
        false
    }

    fn receive_wake(
        &mut self,
        reply: sentinel_wire::Reply,
        scheduler: &mut Scheduler<arch::TrapFrame>,
        processes: &Processes,
    ) {
        let Some(mut ticket) = self.wake else {
            return;
        };
        if reply.monitor != ticket.token
            || reply.beats != ticket.delay_ms
            || reply.age >> 48 != 0
            || matches!(ticket.phase, WakePhase::Fired(_))
        {
            return;
        }
        // Read actual SYSTIMER elapsed time, not the requested delay or delivered IRQ count.
        // SAFETY: MIE=0, hart0 has exclusive ownership of SYSTIMER snapshot.
        let now = Ticks(unsafe { soc::timer::uptime_ticks() }.expect("SYSTIMER wake snapshot"));
        let elapsed = now.saturating_sub(ticket.started).min(u64::from(u32::MAX)) as u32;
        ticket.phase = WakePhase::Fired(elapsed);
        self.wake = Some(ticket);
        let alive = processes.find_alive(ticket.owner).is_some();
        if !alive || Self::finish_wake_waiter(scheduler, processes, ticket, ticket.token, elapsed) {
            self.wake = None;
        }
        // Otherwise retain the event until WAIT. An IRQ between ARM ACK and
        // userspace WAIT must not turn into a lost wakeup.
    }

    pub fn handle_reply(
        &mut self,
        reply: sentinel_wire::Reply,
        scheduler: &mut Scheduler<arch::TrapFrame>,
        processes: &Processes,
    ) {
        if reply.published <= self.last_publish {
            return;
        }
        self.last_publish = reply.published;
        match reply.event {
            sentinel_wire::READY if reply.result == sentinel_wire::OK => {
                self.ready = true;
                return;
            }
            sentinel_wire::HEARTBEAT_MISSED
                if reply.request == 0 && reply.result == sentinel_wire::OK =>
            {
                self.notifications = self.notifications.saturating_add(1);
                return;
            }
            sentinel_wire::DIAGNOSTIC_EVENT
                if reply.request == 0
                    && reply.result != 0
                    && (reply.age as u32 >> 16) == u32::from(sentinel_wire::LOG_HP_EVENT) =>
            {
                self.diagnostics = self.diagnostics.saturating_add(1);
                return;
            }
            sentinel_wire::WAKE_EVENT
                if reply.request == 0 && reply.result == sentinel_wire::OK =>
            {
                self.receive_wake(reply, scheduler, processes);
                return;
            }
            _ => (),
        }
        if !self.pending || reply.request != self.sequence {
            return;
        }
        let command = self.command;
        let expected = match command {
            sentinel_wire::GET_STATUS => sentinel_wire::STATUS,
            sentinel_wire::LOG_META => sentinel_wire::LOG_METADATA,
            sentinel_wire::LOG_READ => sentinel_wire::LOG_ENTRY,
            _ => sentinel_wire::ACK,
        };
        // LOG_ENTRY.result contains record sequence, not a result code.
        let error = reply.event == sentinel_wire::ACK && reply.result != sentinel_wire::OK;
        if reply.event != expected && !error {
            return;
        }
        self.pending = false;
        if command == sentinel_wire::HEARTBEAT {
            self.heartbeat_link.acknowledge(reply.request);
        }
        let error_code = match reply.result {
            sentinel_wire::LOG_GONE | sentinel_wire::LOG_EXHAUSTED => Errno::Eoverflow,
            sentinel_wire::LOG_UNKNOWN => Errno::Enoent,
            sentinel_wire::LOG_BUSY => Errno::Ebusy,
            sentinel_wire::WAKE_ALREADY => Errno::Ealready,
            _ => Errno::Einval,
        };
        if matches!(
            command,
            sentinel_wire::SET_WAKEUP | sentinel_wire::PREPARE_SLEEP
        ) {
            if let Some(mut ticket) = self.wake {
                if ticket.token == reply.request {
                    if error {
                        Self::finish_wake_waiter(
                            scheduler,
                            processes,
                            ticket,
                            error_code.to_abi(),
                            0,
                        );
                        self.wake = None;
                    } else if matches!(ticket.phase, WakePhase::Requested) {
                        ticket.phase = WakePhase::Armed;
                        self.wake = Some(ticket);
                    }
                }
            }
        } else if matches!(
            command,
            sentinel_wire::CANCEL_WAKEUP | sentinel_wire::CANCEL_SLEEP
        ) && !error
        {
            if let Some(ticket) = self.wake {
                if ticket.token == self.argument {
                    Self::finish_wake_waiter(
                        scheduler,
                        processes,
                        ticket,
                        Errno::Ecanceled.to_abi(),
                        0,
                    );
                    self.wake = None;
                }
            }
        }
        let Some(i) = scheduler.tasks.iter().position(|t| {
            t.state == State::Blocked
                && matches!(t.wait, Wait::Sentinel { sequence, .. } if sequence == reply.request)
        }) else {
            return;
        };
        let thread = ThreadSlot(i);
        let Wait::Sentinel { pointer, .. } = scheduler.task(thread).wait else {
            unreachable!()
        };
        let result = if error {
            error_code.to_abi()
        } else if pointer == 0 {
            reply.request
        } else {
            let values = match command {
                sentinel_wire::GET_STATUS => {
                    let age_ms = reply.age * 1000 / u64::from(self.hz);
                    [
                        reply.monitor & 3,
                        age_ms as u32,
                        (age_ms >> 32) as u32,
                        self.hz,
                        reply.beats,
                        reply.monitor >> 2,
                    ]
                }
                sentinel_wire::LOG_META => [
                    self.epoch.unwrap_or(0),
                    reply.monitor,
                    reply.age as u32,
                    (reply.age >> 32) as u32,
                    reply.beats,
                    self.hz,
                ],
                sentinel_wire::LOG_READ => [
                    reply.result,
                    (reply.age as u32) >> 16,
                    reply.monitor,
                    (reply.age as u32) & 0xffff,
                    (reply.age >> 32) as u32,
                    reply.beats,
                ],
                _ => unreachable!(),
            };
            let process = scheduler.task(thread).process;
            if command == sentinel_wire::LOG_READ && values[0] != self.argument {
                scheduler.task_mut(thread).context.registers[8] = Errno::Einval.to_abi();
                scheduler.wake(thread);
                return;
            }
            let mut buffer = [0u8; sentinel_wire::LOG_BYTES];
            for (index, value) in values.into_iter().enumerate() {
                buffer[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
            }
            match copy_to_user(&processes.slot(process).memory, pointer, &buffer) {
                Ok(()) => sentinel_wire::LOG_BYTES as u32,
                Err(e) => e.to_abi(),
            }
        };
        scheduler.task_mut(thread).context.registers[8] = result;
        scheduler.task_mut(thread).context.registers[9] = if command == sentinel_wire::LOG_META {
            self.diagnostics
        } else {
            self.notifications
        };
        scheduler.wake(thread);
    }
}
