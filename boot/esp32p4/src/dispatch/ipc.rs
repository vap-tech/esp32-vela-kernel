use core::fmt::Write;
use vela_kernel::{
    capability::{self as cap, Object},
    ipc::{self, Message},
    process::{Lifecycle, WRITE},
    sched::TASKS,
    task::{State, Wait},
};

use super::Args;
use crate::{
    arch,
    runtime::{Call, Runtime, create_endpoint, deliver, grant},
    syscall,
};

impl Runtime {
    pub(super) fn sys_ipc_call(&mut self, current: usize, args: Args, switch: &mut bool) -> u32 {
        let length = args.a2 as usize;
        let capacity = args.a4 as usize;
        let timeout = args.a5;
        let thread = self.scheduler.current;
        if length > ipc::MESSAGE_BYTES
            || capacity == 0
            || capacity > ipc::MESSAGE_BYTES
            || timeout > 6000
        {
            return syscall::EINVAL;
        }
        if !self.readable(args.a1 as usize, length) || !self.writable(args.a3 as usize, capacity) {
            return syscall::EFAULT;
        }
        let ep = match self.endpoint(current, args.a0, cap::SEND) {
            Ok(e) => e,
            Err(e) => return e,
        };
        let server_process = self.ipc.owner(ep).unwrap();
        // No self-process RPC: avoid an unbounded dependency on caller-owned locks.
        if server_process == current {
            return (-35i32) as u32;
        }
        let Some(server) = self
            .scheduler
            .tasks
            .iter()
            .position(|t| t.process == server_process && t.state != State::Terminated)
        else {
            return ipc::EPIPE;
        };
        let mut dependency = server;
        for _ in 0..TASKS {
            if dependency == thread {
                return (-35i32) as u32;
            }
            if let Wait::Reply { server, .. } = self.scheduler.tasks[dependency].wait {
                dependency = server;
            } else {
                break;
            }
        }
        if self.calls[thread].generation == 0x7f_ffff {
            return ipc::ENOSPC;
        }
        let generation = self.calls[thread].generation + 1;
        let token = (generation << 8) | (thread as u32 + 1);
        let mut message = Message::EMPTY;
        message.length = length;
        message.reply_token = token;
        for i in 0..length {
            // SAFETY: validated source; copy message before blocking caller.
            message.data[i] = unsafe { (args.a1 as *const u8).add(i).read() };
        }
        if let Err(e) = self.ipc.send(ep, message) {
            return e;
        }
        self.calls[thread] = Call {
            generation,
            active: true,
            endpoint: ep,
            server: None,
        };
        let until = if timeout == 0 {
            0
        } else {
            self.scheduler.snapshot().uptime_ticks + u64::from(timeout)
        };
        self.scheduler.block(Wait::Reply {
            token,
            pointer: args.a3 as usize,
            capacity,
            server,
            until,
        });
        self.complete_receivers(ep, false);
        self.scheduler.inherit();
        *switch = true;
        0
    }

    pub(super) fn sys_ipc_reply(&mut self, current: usize, args: Args, switch: &mut bool) -> u32 {
        let client = (args.a0 & 255).wrapping_sub(1) as usize;
        let Some(call) = self.calls.get(client).copied() else {
            return ipc::EBADF;
        };
        if !call.active || call.generation != args.a0 >> 8 {
            return ipc::EPIPE;
        }
        let Some(server) = call.server else {
            return ipc::EPERM;
        };
        if self.scheduler.tasks[server].process != current {
            return ipc::EPERM;
        }
        let Wait::Reply {
            token,
            pointer,
            capacity,
            ..
        } = self.scheduler.tasks[client].wait
        else {
            return ipc::EPIPE;
        };
        if token != args.a0 {
            return ipc::EPIPE;
        }
        if args.a2 as usize > capacity || args.a2 as usize > ipc::MESSAGE_BYTES {
            return ipc::EMSGSIZE;
        }
        let target = self.scheduler.tasks[client].process;
        if !self.readable(args.a1 as usize, args.a2 as usize)
            || !self.processes.slots[target]
                .memory
                .permits(pointer, capacity, WRITE)
        {
            return syscall::EFAULT;
        }
        let mut message = Message::EMPTY;
        message.length = args.a2 as usize;
        for i in 0..message.length {
            // SAFETY: validated server source and caller destination, sole hart.
            message.data[i] = unsafe { (args.a1 as *const u8).add(i).read() };
        }
        unsafe {
            deliver(pointer, &message);
        }
        self.scheduler.tasks[client].context.registers[8] = args.a2;
        self.calls[client].active = false;
        self.scheduler.wake(client);
        self.scheduler.inherit();
        self.scheduler.prefer(client);
        *switch = self.scheduler.should_preempt();
        0
    }

    pub(super) fn sys_cap_or_mem_grant(&mut self, current: usize, args: Args) -> u32 {
        let peer = match self
            .processes
            .slots
            .iter()
            .position(|p| p.state == Lifecycle::Alive && p.id == args.a1)
        {
            Some(p) if p != current => p,
            _ => return syscall::EINVAL,
        };
        if args.a2 > u16::MAX as u32 {
            return syscall::EINVAL;
        }
        let rights = args.a2 as u16;
        let source = match self.processes.slots[current]
            .capabilities
            .get(args.a0, cap::GRANT)
        {
            Ok(c) => c,
            Err(e) => return e,
        };
        if let Object::Memory { region, .. } = source.object {
            if rights & !(cap::READ | cap::WRITE) != 0 || rights & cap::READ == 0 {
                return syscall::EINVAL;
            }
            let handle = match grant(&mut self.processes, current, args.a0, peer, rights) {
                Ok(h) => h,
                Err(e) => return e,
            };
            if let Err(e) = self.processes.slots[peer]
                .memory
                .insert(region, rights as u8)
            {
                self.processes.slots[peer]
                    .capabilities
                    .close(handle)
                    .unwrap();
                return (-(e as i32)) as u32;
            }
            handle
        } else if args.number == syscall::MEM_GRANT {
            syscall::EINVAL
        } else {
            grant(&mut self.processes, current, args.a0, peer, rights).unwrap_or_else(|e| e)
        }
    }

    pub(super) fn sys_cap_revoke(&mut self, current: usize, args: Args) -> u32 {
        let c = match self.processes.slots[current]
            .capabilities
            .get(args.a0, cap::CONTROL)
        {
            Ok(c) => c,
            Err(e) => return e,
        };
        self.invalidate_object(c.object, Some(current));
        if let Object::Endpoint { index, .. } = c.object {
            for i in 0..TASKS {
                if self.calls[i].active
                    && self.calls[i].endpoint == index
                    && self.scheduler.tasks[i].process != current
                {
                    self.fail_call(i, ipc::EPIPE);
                }
            }
            self.complete_receivers(index, false);
        }
        0
    }

    pub(super) fn sys_endpoint_create(&mut self, current: usize) -> u32 {
        create_endpoint(&mut self.ipc, &mut self.processes, current).unwrap_or_else(|e| e)
    }

    pub(super) fn sys_endpoint_close(&mut self, current: usize, args: Args) -> u32 {
        let capability = match self.processes.slots[current].capabilities.get(args.a0, 0) {
            Ok(c) => c,
            Err(e) => return e,
        };
        let ep = match self.ipc.resolve(capability.object) {
            Ok(ep) => ep,
            Err(e) => return e,
        };
        if capability.rights & cap::CONTROL != 0 {
            for i in 0..TASKS {
                if self.calls[i].active && self.calls[i].endpoint == ep {
                    self.fail_call(i, ipc::EPIPE);
                }
            }
            self.ipc.close(ep);
            self.invalidate_object(capability.object, None);
            self.complete_receivers(ep, true);
        } else {
            self.processes.slots[current]
                .capabilities
                .close(args.a0)
                .unwrap();
            self.complete_receivers(ep, false);
        }
        0
    }

    pub(super) fn sys_send(&mut self, current: usize, args: Args, switch: &mut bool) -> u32 {
        let length = args.a2 as usize;
        if length > ipc::MESSAGE_BYTES {
            return ipc::EMSGSIZE;
        }
        if !self.readable(args.a1 as usize, length) {
            return syscall::EFAULT;
        }
        // Validate namespace/rights before touching sender's data.
        if let Err(e) = self.endpoint(current, args.a0, ipc::SEND) {
            return e;
        }
        let mut message = Message::EMPTY;
        message.length = length;
        for i in 0..length {
            // SAFETY: checked sender mapping, copy into a kernel-owned message.
            message.data[i] = unsafe { (args.a1 as *const u8).add(i).read() };
        }
        match self
            .ipc
            .send(self.endpoint(current, args.a0, ipc::SEND).unwrap(), message)
        {
            Ok(ep) => {
                self.complete_receivers(ep, false);
                self.scheduler.inherit();
                *switch = self.scheduler.should_preempt();
                0
            }
            Err(e) => e,
        }
    }

    pub(super) fn sys_recv(
        &mut self,
        current: usize,
        args: Args,
        frame: &mut arch::TrapFrame,
        switch: &mut bool,
    ) -> u32 {
        let capacity = args.a2 as usize;
        if capacity == 0 || capacity > ipc::MESSAGE_BYTES || args.a3 & !syscall::NONBLOCK != 0 {
            return syscall::EINVAL;
        }
        if !self.writable(args.a1 as usize, capacity) {
            return syscall::EFAULT;
        }
        let ep = match self.endpoint(current, args.a0, ipc::RECEIVE) {
            Ok(ep) => ep,
            Err(e) => return e,
        };
        match self.ipc.receive(ep, capacity) {
            Ok(Some(message)) => {
                self.bind_reply(message.reply_token, self.scheduler.current);
                frame.registers[9] = message.reply_token;
                // SAFETY: checked writable receive buffer. No shared-memory pointers.
                unsafe {
                    deliver(args.a1 as usize, &message);
                }
                message.length as u32
            }
            Err(e) => e,
            Ok(None) if args.a3 & syscall::NONBLOCK != 0 => ipc::EAGAIN,
            Ok(None) => {
                // Check+wait registration+Blocked are one IRQ-masked
                // transaction, so send cannot race and lose the wakeup.
                self.scheduler.block(Wait::Receive {
                    endpoint: ep,
                    handle: args.a0,
                    pointer: args.a1 as usize,
                    capacity,
                });
                *switch = true;
                if !self.logged_block {
                    self.logged_block = true;
                    let _ = writeln!(self.console, "[ipc] recv blocked task={}", current + 1);
                }
                0 // replaced with completed length/error before wakeup
            }
        }
    }
}
