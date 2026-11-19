use vela_kernel::{
    errno::Errno,
    id::{Pid, ThreadSlot, Ticks},
    process::Processes,
    sched::Scheduler,
    sentinel::{RequestKind, RequestLink, Retry, WakePhase, WakeTicket},
    task::{State, Wait},
};

use crate::{arch, runtime::device_wait, sentinel_wire, soc, usermem::copy_to_user};

pub struct SentinelClient {
    epoch: Option<u32>,
    link: RequestLink,
    wake: Option<WakeTicket>,
    diagnostics: u32,
    hz: u32,
    heartbeat: bool,
    next_beat: Ticks,
    notifications: u32,
    last_publish: u32,
}

impl SentinelClient {
    pub const fn new() -> Self {
        Self {
            epoch: None,
            link: RequestLink::new(),
            wake: None,
            diagnostics: 0,
            hz: 0,
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

    pub const fn is_offline(&self) -> bool {
        self.link.is_offline()
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

    pub fn epoch_or_zero(&self) -> u32 {
        self.epoch.unwrap_or(0)
    }

    /// Protocol version once READY, `0` before, `Etimedout` when the LP
    /// request/heartbeat link is offline.
    pub fn link_status(&self) -> Result<u32, Errno> {
        self.link
            .status()
            .map(|ready| if ready { sentinel_wire::VERSION } else { 0 })
    }

    pub fn request(
        &mut self,
        command: u32,
        arg0: u32,
        arg1: u32,
        now: Ticks,
    ) -> Result<u32, Errno> {
        let kind = if command == sentinel_wire::HEARTBEAT {
            RequestKind::Heartbeat
        } else {
            RequestKind::Command(command)
        };
        let sequence = self.link.submit(kind, arg0, now)?;
        // SAFETY: hart0 MIE=0, only one pending request until its exact response.
        unsafe {
            soc::sentinel::request(sequence, command, arg0, arg1);
        }
        Ok(sequence)
    }

    pub fn tick(&mut self, uptime: Ticks) {
        if let Some(retry) = self.link.poll_timeout(uptime) {
            // Keep the slot and payload stable until the original request completes.
            unsafe {
                match retry {
                    Retry::Heartbeat(seq) => soc::sentinel::retry_heartbeat(seq),
                    Retry::Command(cmd) => soc::sentinel::reassert_command(cmd),
                }
            }
            return;
        }
        if self.link.is_ready()
            && self.heartbeat
            && !self
                .wake
                .is_some_and(|t| t.sleep && !matches!(t.phase, WakePhase::Fired(_)))
            && !self.link.is_pending()
            && uptime >= self.next_beat
            && self
                .request(sentinel_wire::HEARTBEAT, 3000, 0, uptime)
                .is_ok()
        {
            self.next_beat = uptime.saturating_add(100);
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
                && matches!(
                    t.wait,
                    Wait::Device {
                        domain: device_wait::SENTINEL_WAKE,
                        token,
                        ..
                    } if token == ticket.token
                )
        }) {
            let thread = ThreadSlot(i);
            scheduler.wake_with2(thread, result, elapsed);
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
                self.link.mark_ready();
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
        let Some(inflight) = self.link.in_flight() else {
            return;
        };
        if reply.request != inflight.sequence {
            return;
        }
        let command = match inflight.kind {
            RequestKind::Heartbeat => sentinel_wire::HEARTBEAT,
            RequestKind::Command(cmd) => cmd,
        };
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
        self.link.complete(reply.request);
        let error_code = match reply.result {
            sentinel_wire::LOG_GONE | sentinel_wire::LOG_EXHAUSTED => Errno::Eoverflow,
            sentinel_wire::LOG_UNKNOWN => Errno::Enoent,
            sentinel_wire::LOG_BUSY => Errno::Ebusy,
            sentinel_wire::WAKE_ALREADY => Errno::Ealready,
            _ => Errno::Einval,
        };
        self.settle_wake_ticket(
            command,
            inflight.argument,
            &reply,
            error.then_some(error_code),
            scheduler,
            processes,
        );
        self.complete_waiter(
            command,
            inflight.argument,
            &reply,
            error.then_some(error_code),
            scheduler,
            processes,
        );
    }

    /// Advance or finish the single wake alarm after the LP acknowledged
    /// `command`. `error` is `Some` when the LP rejected it.
    fn settle_wake_ticket(
        &mut self,
        command: u32,
        argument: u32,
        reply: &sentinel_wire::Reply,
        error: Option<Errno>,
        scheduler: &mut Scheduler<arch::TrapFrame>,
        processes: &Processes,
    ) {
        if matches!(
            command,
            sentinel_wire::SET_WAKEUP | sentinel_wire::PREPARE_SLEEP
        ) {
            let Some(mut ticket) = self.wake else {
                return;
            };
            if ticket.token != reply.request {
                return;
            }
            if let Some(code) = error {
                Self::finish_wake_waiter(scheduler, processes, ticket, code.to_abi(), 0);
                self.wake = None;
            } else if matches!(ticket.phase, WakePhase::Requested) {
                ticket.phase = WakePhase::Armed;
                self.wake = Some(ticket);
            }
        } else if matches!(
            command,
            sentinel_wire::CANCEL_WAKEUP | sentinel_wire::CANCEL_SLEEP
        ) && error.is_none()
        {
            if let Some(ticket) = self.wake {
                if ticket.token == argument {
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
    }

    /// Words copied to userspace for the three data-returning commands.
    fn payload(&self, command: u32, reply: &sentinel_wire::Reply) -> [u32; 6] {
        match command {
            sentinel_wire::GET_STATUS => {
                // Unstarted LP reports hz == 0; never divide by it.
                let age_ms = (reply.age * 1000)
                    .checked_div(u64::from(self.hz))
                    .unwrap_or(0);
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
        }
    }

    /// Complete the thread blocked on this request, if any.
    fn complete_waiter(
        &self,
        command: u32,
        argument: u32,
        reply: &sentinel_wire::Reply,
        error: Option<Errno>,
        scheduler: &mut Scheduler<arch::TrapFrame>,
        processes: &Processes,
    ) {
        let Some(i) = scheduler.tasks.iter().position(|t| {
            t.state == State::Blocked
                && matches!(
                    t.wait,
                    Wait::Device {
                        domain: device_wait::SENTINEL_REQ,
                        token,
                        ..
                    } if token == reply.request
                )
        }) else {
            return;
        };
        let thread = ThreadSlot(i);
        let Wait::Device { pointer, .. } = scheduler.task(thread).wait else {
            unreachable!()
        };
        let result = if let Some(code) = error {
            code.to_abi()
        } else if pointer == 0 {
            reply.request
        } else {
            let values = self.payload(command, reply);
            if command == sentinel_wire::LOG_READ && values[0] != argument {
                scheduler.wake_with(thread, Errno::Einval.to_abi());
                return;
            }
            let process = scheduler.task(thread).process;
            let mut buffer = [0u8; sentinel_wire::LOG_BYTES];
            for (index, value) in values.into_iter().enumerate() {
                buffer[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
            }
            match copy_to_user(&processes.slot(process).memory, pointer, &buffer) {
                Ok(()) => sentinel_wire::LOG_BYTES as u32,
                Err(e) => e.to_abi(),
            }
        };
        let extra = if command == sentinel_wire::LOG_META {
            self.diagnostics
        } else {
            self.notifications
        };
        scheduler.wake_with2(thread, result, extra);
    }
}
