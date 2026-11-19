use vela_kernel::{
    capability as cap,
    sentinel::{WakePhase, WakeTicket},
    task::{State, Wait},
};

use super::Args;
use crate::{arch, runtime::Runtime, sentinel_wire, soc, syscall};

impl Runtime {
    pub(super) fn sys_sentinel_control(&mut self, current: usize, args: Args) -> u32 {
        if self.processes.slots[current].id != 2 {
            return cap::EPERM;
        }
        if args.a0 > 1 {
            return syscall::EINVAL;
        }
        self.sentinel_heartbeat = args.a0 != 0;
        if self.sentinel_heartbeat {
            self.sentinel_next_beat = 0;
        }
        0
    }

    pub(super) fn sys_sentinel_arm_or_prepare(
        &mut self,
        current: usize,
        args: Args,
        switch: &mut bool,
    ) -> u32 {
        let sleep = args.number == syscall::SENTINEL_PREPARE_SLEEP;
        let owner = self.processes.slots[current].id;
        if owner != 2 {
            return cap::EPERM;
        }
        if !(1..=60_000).contains(&args.a0) {
            return syscall::EINVAL;
        }
        if self.sentinel_wake.is_some() {
            return (-16i32) as u32;
        }
        // SAFETY: sole hart snapshot owner under masked IRQs.
        let now = unsafe { soc::timer::uptime_ticks() }.expect("SYSTIMER arm snapshot");
        let sequence = match self.sentinel_request(
            if sleep {
                sentinel_wire::PREPARE_SLEEP
            } else {
                sentinel_wire::SET_WAKEUP
            },
            args.a0,
            0,
        ) {
            Ok(seq) => seq,
            Err(e) => return e,
        };
        self.sentinel_wake = Some(WakeTicket {
            token: sequence,
            owner,
            started: now,
            expires: now + (u64::from(args.a0) + 9) / 10 + 100,
            delay_ms: args.a0,
            sleep,
            phase: WakePhase::Requested,
        });
        self.scheduler.block(Wait::Sentinel {
            sequence,
            deadline: now + 100,
            pointer: 0,
        });
        *switch = true;
        0
    }

    pub(super) fn sys_sentinel_wait_wake(
        &mut self,
        current: usize,
        args: Args,
        frame: &mut arch::TrapFrame,
        switch: &mut bool,
    ) -> u32 {
        let Some(ticket) = self.sentinel_wake else {
            return (-2i32) as u32;
        };
        if ticket.owner != self.processes.slots[current].id {
            return cap::EPERM;
        }
        if args.a0 != 0 && args.a0 != ticket.token {
            return (-2i32) as u32;
        }
        if let WakePhase::Fired(elapsed) = ticket.phase {
            self.sentinel_wake = None;
            frame.registers[9] = elapsed;
            return ticket.token;
        }
        if self.scheduler.tasks.iter().any(|t| {
            t.state == State::Blocked
                && matches!(t.wait, Wait::SentinelWake { token, .. } if token == ticket.token)
        }) {
            return (-16i32) as u32;
        }
        // SAFETY: masked hart0 snapshot owner, no scheduler references escape.
        let now = unsafe { soc::timer::uptime_ticks() }.expect("SYSTIMER wait snapshot");
        if now >= ticket.expires {
            return (-110i32) as u32;
        }
        self.scheduler.block(Wait::SentinelWake {
            token: ticket.token,
            deadline: ticket.expires,
        });
        *switch = true;
        0
    }

    pub(super) fn sys_sentinel_cancel_wake(
        &mut self,
        current: usize,
        args: Args,
        switch: &mut bool,
    ) -> u32 {
        let Some(ticket) = self.sentinel_wake else {
            return (-2i32) as u32;
        };
        if ticket.owner != self.processes.slots[current].id {
            return cap::EPERM;
        }
        if args.a0 != 0 && args.a0 != ticket.token {
            return (-2i32) as u32;
        }
        if matches!(ticket.phase, WakePhase::Fired(_)) {
            return (-114i32) as u32;
        }
        let sequence = match self.sentinel_request(
            if ticket.sleep {
                sentinel_wire::CANCEL_SLEEP
            } else {
                sentinel_wire::CANCEL_WAKEUP
            },
            ticket.token,
            0,
        ) {
            Ok(seq) => seq,
            Err(e) => return e,
        };
        self.scheduler.block(Wait::Sentinel {
            sequence,
            deadline: self.scheduler.snapshot().uptime_ticks + 100,
            pointer: 0,
        });
        *switch = true;
        0
    }

    pub(super) fn sys_sentinel_command(
        &mut self,
        current: usize,
        args: Args,
        switch: &mut bool,
    ) -> u32 {
        if self.processes.slots[current].id != 2 {
            return cap::EPERM;
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
            return syscall::EFAULT;
        }
        let sequence = match self.sentinel_request(command, arg0, arg1) {
            Ok(seq) => seq,
            Err(e) => return e,
        };
        let deadline = self.scheduler.snapshot().uptime_ticks.saturating_add(100);
        self.scheduler.block(Wait::Sentinel {
            sequence,
            deadline,
            pointer,
        });
        *switch = true;
        0
    }

    pub(super) fn sys_sentinel_status(&self, frame: &mut arch::TrapFrame) -> u32 {
        frame.registers[9] = self.sentinel_epoch.unwrap_or(0);
        if self.heartbeat_link.offline {
            (-110i32) as u32
        } else if self.sentinel_ready {
            sentinel_wire::VERSION
        } else {
            0
        }
    }
}
