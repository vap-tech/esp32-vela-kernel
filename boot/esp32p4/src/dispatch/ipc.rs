use core::fmt::Write;
use vela_kernel::{
    capability::{self as cap, Object},
    errno::Errno,
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
    pub(super) fn sys_ipc_call(
        &mut self,
        current: usize,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let length = args.a2 as usize;
        let capacity = args.a4 as usize;
        let timeout = args.a5;
        let thread = self.scheduler.current;
        if length > ipc::MESSAGE_BYTES
            || capacity == 0
            || capacity > ipc::MESSAGE_BYTES
            || timeout > 6000
        {
            return Err(Errno::Einval);
        }
        if !self.readable(args.a1 as usize, length) || !self.writable(args.a3 as usize, capacity) {
            return Err(Errno::Efault);
        }
        let ep = self.endpoint(current, args.a0, cap::SEND)?;
        let server_process = self.ipc.owner(ep).unwrap();
        // No self-process RPC: avoid an unbounded dependency on caller-owned locks.
        if server_process == current {
            return Err(Errno::Edeadlk);
        }
        let server = self
            .scheduler
            .tasks
            .iter()
            .position(|t| t.process == server_process && t.state != State::Terminated)
            .ok_or(Errno::Epipe)?;
        let mut dependency = server;
        for _ in 0..TASKS {
            if dependency == thread {
                return Err(Errno::Edeadlk);
            }
            if let Wait::Reply { server, .. } = self.scheduler.tasks[dependency].wait {
                dependency = server;
            } else {
                break;
            }
        }
        if self.calls[thread].generation == 0x7f_ffff {
            return Err(Errno::Enospc);
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
        self.ipc.send(ep, message)?;
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
        Ok(0)
    }

    pub(super) fn sys_ipc_reply(
        &mut self,
        current: usize,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let client = (args.a0 & 255).wrapping_sub(1) as usize;
        let call = self.calls.get(client).copied().ok_or(Errno::Ebadf)?;
        if !call.active || call.generation != args.a0 >> 8 {
            return Err(Errno::Epipe);
        }
        let server = call.server.ok_or(Errno::Eperm)?;
        if self.scheduler.tasks[server].process != current {
            return Err(Errno::Eperm);
        }
        let Wait::Reply {
            token,
            pointer,
            capacity,
            ..
        } = self.scheduler.tasks[client].wait
        else {
            return Err(Errno::Epipe);
        };
        if token != args.a0 {
            return Err(Errno::Epipe);
        }
        if args.a2 as usize > capacity || args.a2 as usize > ipc::MESSAGE_BYTES {
            return Err(Errno::Emsgsize);
        }
        let target = self.scheduler.tasks[client].process;
        if !self.readable(args.a1 as usize, args.a2 as usize)
            || !self.processes.slots[target]
                .memory
                .permits(pointer, capacity, WRITE)
        {
            return Err(Errno::Efault);
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
        Ok(0)
    }

    pub(super) fn sys_cap_or_mem_grant(
        &mut self,
        current: usize,
        args: Args,
    ) -> Result<u32, Errno> {
        let peer = match self
            .processes
            .slots
            .iter()
            .position(|p| p.state == Lifecycle::Alive && p.id == args.a1)
        {
            Some(p) if p != current => p,
            _ => return Err(Errno::Einval),
        };
        if args.a2 > u16::MAX as u32 {
            return Err(Errno::Einval);
        }
        let rights = args.a2 as u16;
        let source = self.processes.slots[current]
            .capabilities
            .get(args.a0, cap::GRANT)?;
        if let Object::Memory { region, .. } = source.object {
            if rights & !(cap::READ | cap::WRITE) != 0 || rights & cap::READ == 0 {
                return Err(Errno::Einval);
            }
            let handle = grant(&mut self.processes, current, args.a0, peer, rights)?;
            if let Err(e) = self.processes.slots[peer]
                .memory
                .insert(region, rights as u8)
            {
                self.processes.slots[peer]
                    .capabilities
                    .close(handle)
                    .unwrap();
                return Err(e);
            }
            Ok(handle)
        } else if args.number == syscall::MEM_GRANT {
            Err(Errno::Einval)
        } else {
            grant(&mut self.processes, current, args.a0, peer, rights)
        }
    }

    pub(super) fn sys_cap_revoke(&mut self, current: usize, args: Args) -> Result<u32, Errno> {
        let c = self.processes.slots[current]
            .capabilities
            .get(args.a0, cap::CONTROL)?;
        self.invalidate_object(c.object, Some(current));
        if let Object::Endpoint { index, .. } = c.object {
            for i in 0..TASKS {
                if self.calls[i].active
                    && self.calls[i].endpoint == index
                    && self.scheduler.tasks[i].process != current
                {
                    self.fail_call(i, Errno::Epipe);
                }
            }
            self.complete_receivers(index, false);
        }
        Ok(0)
    }

    pub(super) fn sys_endpoint_create(&mut self, current: usize) -> Result<u32, Errno> {
        create_endpoint(&mut self.ipc, &mut self.processes, current)
    }

    pub(super) fn sys_endpoint_close(&mut self, current: usize, args: Args) -> Result<u32, Errno> {
        let capability = self.processes.slots[current].capabilities.get(args.a0, 0)?;
        let ep = self.ipc.resolve(capability.object)?;
        if capability.rights & cap::CONTROL != 0 {
            for i in 0..TASKS {
                if self.calls[i].active && self.calls[i].endpoint == ep {
                    self.fail_call(i, Errno::Epipe);
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
        Ok(0)
    }

    pub(super) fn sys_send(
        &mut self,
        current: usize,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let length = args.a2 as usize;
        if length > ipc::MESSAGE_BYTES {
            return Err(Errno::Emsgsize);
        }
        if !self.readable(args.a1 as usize, length) {
            return Err(Errno::Efault);
        }
        // Validate namespace/rights before touching sender's data.
        let ep = self.endpoint(current, args.a0, ipc::SEND)?;
        let mut message = Message::EMPTY;
        message.length = length;
        for i in 0..length {
            // SAFETY: checked sender mapping, copy into a kernel-owned message.
            message.data[i] = unsafe { (args.a1 as *const u8).add(i).read() };
        }
        let ep = self.ipc.send(ep, message)?;
        self.complete_receivers(ep, false);
        self.scheduler.inherit();
        *switch = self.scheduler.should_preempt();
        Ok(0)
    }

    pub(super) fn sys_recv(
        &mut self,
        current: usize,
        args: Args,
        frame: &mut arch::TrapFrame,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let capacity = args.a2 as usize;
        if capacity == 0 || capacity > ipc::MESSAGE_BYTES || args.a3 & !syscall::NONBLOCK != 0 {
            return Err(Errno::Einval);
        }
        if !self.writable(args.a1 as usize, capacity) {
            return Err(Errno::Efault);
        }
        let ep = self.endpoint(current, args.a0, ipc::RECEIVE)?;
        match self.ipc.receive(ep, capacity)? {
            Some(message) => {
                self.bind_reply(message.reply_token, self.scheduler.current);
                frame.registers[9] = message.reply_token;
                // SAFETY: checked writable receive buffer. No shared-memory pointers.
                unsafe {
                    deliver(args.a1 as usize, &message);
                }
                Ok(message.length as u32)
            }
            None if args.a3 & syscall::NONBLOCK != 0 => Err(Errno::Eagain),
            None => {
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
                Ok(0) // replaced with completed length/error before wakeup
            }
        }
    }
}
