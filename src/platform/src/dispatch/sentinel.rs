use vela_kernel::{
    capability::{self as cap, Authority},
    errno::Errno,
    id::{Handle, ProcSlot, Ticks},
    lifecycle,
    sentinel::{WakePhase, WakeTicket},
    task::{Context, State, Wait},
};

use super::Args;
use crate::{
    arch,
    runtime::{Runtime, device_wait},
    sentinel_wire, soc, syscall,
};

impl Runtime {
    pub(super) fn sys_sentinel_control(
        &mut self,
        current: ProcSlot,
        args: Args,
    ) -> Result<u32, Errno> {
        lifecycle::require_authority(
            &self.processes,
            current,
            Handle(args.a0),
            Authority::Sentinel,
            cap::CONTROL,
        )?;
        if args.a1 > 1 {
            return Err(Errno::Einval);
        }
        self.sentinel.set_heartbeat(args.a1 != 0);
        Ok(0)
    }

    pub(super) fn sys_sentinel_arm_or_prepare(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let sleep = args.number == syscall::SENTINEL_PREPARE_SLEEP;
        let rights = if sleep {
            cap::WAIT | cap::CONTROL
        } else {
            cap::WAIT
        };
        lifecycle::require_authority(
            &self.processes,
            current,
            Handle(args.a0),
            Authority::Sentinel,
            rights,
        )?;
        let delay_ms = args.a1;
        if !(1..=60_000).contains(&delay_ms) {
            return Err(Errno::Einval);
        }
        if self.sentinel.wake_ticket().is_some() {
            return Err(Errno::Ebusy);
        }
        let owner = self.processes.slot(current).id;
        // SAFETY: sole hart snapshot owner under masked IRQs.
        let now = Ticks(unsafe { soc::timer::uptime_ticks() }.expect("SYSTIMER arm snapshot"));
        let sequence = self.sentinel.request(
            if sleep {
                sentinel_wire::PREPARE_SLEEP
            } else {
                sentinel_wire::SET_WAKEUP
            },
            delay_ms,
            0,
            now,
        )?;
        self.sentinel.set_wake_ticket(Some(WakeTicket {
            token: sequence,
            owner,
            started: now,
            expires: now.saturating_add((u64::from(delay_ms) + 9) / 10 + 100),
            delay_ms,
            sleep,
            phase: WakePhase::Requested,
        }));
        self.scheduler.block_until(
            Wait::Device {
                domain: device_wait::SENTINEL_REQ,
                token: sequence,
                pointer: 0,
            },
            now.saturating_add(100),
        );
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
        lifecycle::require_authority(
            &self.processes,
            current,
            Handle(args.a0),
            Authority::Sentinel,
            cap::WAIT,
        )?;
        let ticket = self.sentinel.wake_ticket().ok_or(Errno::Enoent)?;
        if ticket.owner != self.processes.slot(current).id {
            return Err(Errno::Eperm);
        }
        if args.a1 != 0 && args.a1 != ticket.token {
            return Err(Errno::Enoent);
        }
        if let WakePhase::Fired(elapsed) = ticket.phase {
            self.sentinel.set_wake_ticket(None);
            frame.set_return2(elapsed);
            return Ok(ticket.token);
        }
        if matches!(ticket.phase, WakePhase::Requested) && self.sentinel.is_offline() {
            return Err(Errno::Etimedout);
        }
        if self.scheduler.tasks.iter().any(|t| {
            t.state == State::Blocked
                && matches!(
                    t.wait,
                    Wait::Device {
                        domain: device_wait::SENTINEL_WAKE,
                        token,
                        ..
                    } if token == ticket.token
                )
        }) {
            return Err(Errno::Ebusy);
        }
        // SAFETY: masked hart0 snapshot owner, no scheduler references escape.
        let now = Ticks(unsafe { soc::timer::uptime_ticks() }.expect("SYSTIMER wait snapshot"));
        if now >= ticket.expires {
            return Err(Errno::Etimedout);
        }
        frame.set_return2(0);
        self.scheduler.block_until(
            Wait::Device {
                domain: device_wait::SENTINEL_WAKE,
                token: ticket.token,
                pointer: 0,
            },
            ticket.expires,
        );
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_sentinel_cancel_wake(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        lifecycle::require_authority(
            &self.processes,
            current,
            Handle(args.a0),
            Authority::Sentinel,
            cap::WAIT,
        )?;
        let ticket = self.sentinel.wake_ticket().ok_or(Errno::Enoent)?;
        if ticket.owner != self.processes.slot(current).id {
            return Err(Errno::Eperm);
        }
        if args.a1 != 0 && args.a1 != ticket.token {
            return Err(Errno::Enoent);
        }
        if matches!(ticket.phase, WakePhase::Fired(_)) {
            return Err(Errno::Ealready);
        }
        let now = self.scheduler.uptime();
        let sequence = self.sentinel.request(
            if ticket.sleep {
                sentinel_wire::CANCEL_SLEEP
            } else {
                sentinel_wire::CANCEL_WAKEUP
            },
            ticket.token,
            0,
            now,
        )?;
        self.scheduler.block_until(
            Wait::Device {
                domain: device_wait::SENTINEL_REQ,
                token: sequence,
                pointer: 0,
            },
            now.saturating_add(100),
        );
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_sentinel_command(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let rights = if args.number == syscall::SENTINEL_DIAG {
            cap::WRITE
        } else {
            cap::READ
        };
        lifecycle::require_authority(
            &self.processes,
            current,
            Handle(args.a0),
            Authority::Sentinel,
            rights,
        )?;
        let (command, arg0, arg1, pointer, size) = match args.number {
            syscall::SENTINEL_QUERY => (
                sentinel_wire::GET_STATUS,
                0,
                0,
                args.a1 as usize,
                args.a2 as usize,
            ),
            syscall::SENTINEL_DIAG => (sentinel_wire::DIAGNOSTIC, args.a1, args.a2, 0, 0),
            syscall::SENTINEL_LOG => (
                if args.a1 == 0 {
                    sentinel_wire::LOG_META
                } else {
                    sentinel_wire::LOG_READ
                },
                args.a1,
                0,
                args.a2 as usize,
                args.a3 as usize,
            ),
            _ => (sentinel_wire::HELLO, 0, 0, 0, 0),
        };
        let data = command == sentinel_wire::GET_STATUS
            || command == sentinel_wire::LOG_META
            || command == sentinel_wire::LOG_READ;
        if data && (size != sentinel_wire::LOG_BYTES || !self.writable(pointer, size)) {
            return Err(Errno::Efault);
        }
        let now = self.scheduler.uptime();
        let sequence = self.sentinel.request(command, arg0, arg1, now)?;
        let deadline = now.saturating_add(100);
        self.scheduler.block_until(
            Wait::Device {
                domain: device_wait::SENTINEL_REQ,
                token: sequence,
                pointer,
            },
            deadline,
        );
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_sentinel_status(
        &self,
        current: ProcSlot,
        args: Args,
        frame: &mut arch::TrapFrame,
    ) -> Result<u32, Errno> {
        lifecycle::require_authority(
            &self.processes,
            current,
            Handle(args.a0),
            Authority::Sentinel,
            cap::READ,
        )?;
        frame.set_return2(self.sentinel.epoch_or_zero());
        self.sentinel.link_status()
    }
}
