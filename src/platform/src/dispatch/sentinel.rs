use vela_kernel::{
    errno::Errno,
    id::{Pid, ProcSlot, Ticks},
    sentinel::{WakePhase, WakeTicket},
    task::{Context, State, Wait},
};

use super::Args;
use crate::{arch, runtime::Runtime, sentinel_wire, soc, syscall};

impl Runtime {
    pub(super) fn sys_sentinel_control(
        &mut self,
        current: ProcSlot,
        args: Args,
    ) -> Result<u32, Errno> {
        if self.processes.slot(current).id != Pid(2) {
            return Err(Errno::Eperm);
        }
        if args.a0 > 1 {
            return Err(Errno::Einval);
        }
        self.sentinel.set_heartbeat(args.a0 != 0);
        Ok(0)
    }

    pub(super) fn sys_sentinel_arm_or_prepare(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let sleep = args.number == syscall::SENTINEL_PREPARE_SLEEP;
        let owner = self.processes.slot(current).id;
        if owner != Pid(2) {
            return Err(Errno::Eperm);
        }
        if !(1..=60_000).contains(&args.a0) {
            return Err(Errno::Einval);
        }
        if self.sentinel.wake_ticket().is_some() {
            return Err(Errno::Ebusy);
        }
        // SAFETY: sole hart snapshot owner under masked IRQs.
        let now = Ticks(unsafe { soc::timer::uptime_ticks() }.expect("SYSTIMER arm snapshot"));
        let sequence = self.sentinel.request(
            if sleep {
                sentinel_wire::PREPARE_SLEEP
            } else {
                sentinel_wire::SET_WAKEUP
            },
            args.a0,
            0,
        )?;
        self.sentinel.set_wake_ticket(Some(WakeTicket {
            token: sequence,
            owner,
            started: now,
            expires: now.saturating_add((u64::from(args.a0) + 9) / 10 + 100),
            delay_ms: args.a0,
            sleep,
            phase: WakePhase::Requested,
        }));
        self.scheduler.block(Wait::Sentinel {
            sequence,
            deadline: now.saturating_add(100),
            pointer: 0,
        });
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_sentinel_wait_wake(
        &mut self,
        current: ProcSlot,
        args: Args,
        frame: &mut arch::TrapFrame,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let ticket = self.sentinel.wake_ticket().ok_or(Errno::Enoent)?;
        if ticket.owner != self.processes.slot(current).id {
            return Err(Errno::Eperm);
        }
        if args.a0 != 0 && args.a0 != ticket.token {
            return Err(Errno::Enoent);
        }
        if let WakePhase::Fired(elapsed) = ticket.phase {
            self.sentinel.set_wake_ticket(None);
            frame.set_return2(elapsed);
            return Ok(ticket.token);
        }
        if self.scheduler.tasks.iter().any(|t| {
            t.state == State::Blocked
                && matches!(t.wait, Wait::SentinelWake { token, .. } if token == ticket.token)
        }) {
            return Err(Errno::Ebusy);
        }
        // SAFETY: masked hart0 snapshot owner, no scheduler references escape.
        let now = Ticks(unsafe { soc::timer::uptime_ticks() }.expect("SYSTIMER wait snapshot"));
        if now >= ticket.expires {
            return Err(Errno::Etimedout);
        }
        self.scheduler.block(Wait::SentinelWake {
            token: ticket.token,
            deadline: ticket.expires,
        });
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_sentinel_cancel_wake(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let ticket = self.sentinel.wake_ticket().ok_or(Errno::Enoent)?;
        if ticket.owner != self.processes.slot(current).id {
            return Err(Errno::Eperm);
        }
        if args.a0 != 0 && args.a0 != ticket.token {
            return Err(Errno::Enoent);
        }
        if matches!(ticket.phase, WakePhase::Fired(_)) {
            return Err(Errno::Ealready);
        }
        let sequence = self.sentinel.request(
            if ticket.sleep {
                sentinel_wire::CANCEL_SLEEP
            } else {
                sentinel_wire::CANCEL_WAKEUP
            },
            ticket.token,
            0,
        )?;
        self.scheduler.block(Wait::Sentinel {
            sequence,
            deadline: self.scheduler.uptime().saturating_add(100),
            pointer: 0,
        });
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_sentinel_command(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        if self.processes.slot(current).id != Pid(2) {
            return Err(Errno::Eperm);
        }
        let (command, arg0, arg1, pointer, size) = match args.number {
            syscall::SENTINEL_QUERY => (
                sentinel_wire::GET_STATUS,
                0,
                0,
                args.a0 as usize,
                args.a1 as usize,
            ),
            syscall::SENTINEL_DIAG => (sentinel_wire::DIAGNOSTIC, args.a0, args.a1, 0, 0),
            syscall::SENTINEL_LOG => (
                if args.a0 == 0 {
                    sentinel_wire::LOG_META
                } else {
                    sentinel_wire::LOG_READ
                },
                args.a0,
                0,
                args.a1 as usize,
                args.a2 as usize,
            ),
            _ => (sentinel_wire::HELLO, 0, 0, 0, 0),
        };
        let data = command == sentinel_wire::GET_STATUS
            || command == sentinel_wire::LOG_META
            || command == sentinel_wire::LOG_READ;
        if data && (size != sentinel_wire::LOG_BYTES || !self.writable(pointer, size)) {
            return Err(Errno::Efault);
        }
        let sequence = self.sentinel.request(command, arg0, arg1)?;
        let deadline = self.scheduler.uptime().saturating_add(100);
        self.scheduler.block(Wait::Sentinel {
            sequence,
            deadline,
            pointer,
        });
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_sentinel_status(&self, frame: &mut arch::TrapFrame) -> Result<u32, Errno> {
        frame.set_return2(self.sentinel.epoch_or_zero());
        self.sentinel.link_status()
    }
}
