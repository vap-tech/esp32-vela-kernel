use core::fmt::Write;
use vela_kernel::{
    capability::{self as cap, Object},
    errno::Errno,
    id::{Handle, Pid, ProcSlot, ThreadSlot},
    ipc::{self, Message},
    sched::TASKS,
    task::{State, Wait},
};

use super::Args;
use crate::{
    arch,
    runtime::{Call, Runtime, create_endpoint, grant},
    syscall,
    usermem::{copy_from_user, copy_to_user_within},
};

impl Runtime {
    pub(super) fn sys_ipc_call(
        &mut self,
        current: ProcSlot,
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
        let ep = self.endpoint(current, Handle(args.a0), cap::SEND)?;
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
            .map(ThreadSlot)
            .ok_or(Errno::Epipe)?;
        let mut dependency = server;
        for _ in 0..TASKS {
            if dependency == thread {
                return Err(Errno::Edeadlk);
            }
            if let Wait::Reply { server, .. } = self.scheduler.task(dependency).wait {
                dependency = server;
            } else {
                break;
            }
        }
        if self.calls[thread.index()].generation == 0x7f_ffff {
            return Err(Errno::Enospc);
        }
        let generation = self.calls[thread.index()].generation + 1;
        let token = (generation << 8) | (thread.index() as u32 + 1);
        let mut message = Message::EMPTY;
        message.length = length;
        message.reply_token = token;
        copy_from_user(self.memory(), args.a1 as usize, &mut message.data[..length])?;
        self.ipc.send(ep, message)?;
        self.calls[thread.index()] = Call {
            generation,
            active: true,
            endpoint: ep,
            server: None,
        };
        let until = if timeout == 0 {
            None
        } else {
            Some(self.scheduler.uptime().saturating_add(u64::from(timeout)))
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
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let client_idx = (args.a0 & 255).wrapping_sub(1) as usize;
        let call = self.calls.get(client_idx).copied().ok_or(Errno::Ebadf)?;
        let client = ThreadSlot(client_idx);
        if !call.active || call.generation != args.a0 >> 8 {
            return Err(Errno::Epipe);
        }
        let server = call.server.ok_or(Errno::Eperm)?;
        if self.scheduler.task(server).process != current {
            return Err(Errno::Eperm);
        }
        let Wait::Reply {
            token,
            pointer,
            capacity,
            ..
        } = self.scheduler.task(client).wait
        else {
            return Err(Errno::Epipe);
        };
        if token != args.a0 {
            return Err(Errno::Epipe);
        }
        let length = args.a2 as usize;
        if length > capacity || length > ipc::MESSAGE_BYTES {
            return Err(Errno::Emsgsize);
        }
        let target = self.scheduler.task(client).process;
        let mut message = Message::EMPTY;
        message.length = length;
        copy_from_user(self.memory(), args.a1 as usize, &mut message.data[..length])?;
        copy_to_user_within(
            &self.processes.slot(target).memory,
            pointer,
            capacity,
            &message.data[..length],
        )?;
        self.scheduler.task_mut(client).context.registers[8] = args.a2;
        self.calls[client.index()].active = false;
        self.scheduler.wake(client);
        self.scheduler.inherit();
        self.scheduler.prefer(client);
        *switch = self.scheduler.should_preempt();
        Ok(0)
    }

    pub(super) fn sys_cap_or_mem_grant(
        &mut self,
        current: ProcSlot,
        args: Args,
    ) -> Result<u32, Errno> {
        let peer = match self.processes.find_alive(Pid(args.a1)) {
            Some(p) if p != current => p,
            _ => return Err(Errno::Einval),
        };
        if args.a2 > u16::MAX as u32 {
            return Err(Errno::Einval);
        }
        let rights = args.a2 as u16;
        let source_handle = Handle(args.a0);
        let source = self
            .processes
            .slot(current)
            .capabilities
            .get(source_handle, cap::GRANT)?;
        if let Object::Memory { region, .. } = source.object {
            if rights & !(cap::READ | cap::WRITE) != 0 || rights & cap::READ == 0 {
                return Err(Errno::Einval);
            }
            let handle = grant(&mut self.processes, current, source_handle, peer, rights)?;
            if let Err(e) = self
                .processes
                .slot_mut(peer)
                .memory
                .insert(region, rights as u8)
            {
                self.processes
                    .slot_mut(peer)
                    .capabilities
                    .close(handle)
                    .unwrap();
                return Err(e);
            }
            Ok(handle.raw())
        } else if args.number == syscall::MEM_GRANT {
            Err(Errno::Einval)
        } else {
            grant(&mut self.processes, current, source_handle, peer, rights).map(Handle::raw)
        }
    }

    pub(super) fn sys_cap_revoke(&mut self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        let c = self
            .processes
            .slot(current)
            .capabilities
            .get(Handle(args.a0), cap::CONTROL)?;
        self.invalidate_object(c.object, Some(current));
        if let Object::Endpoint { index, .. } = c.object {
            for i in 0..TASKS {
                let thread = ThreadSlot(i);
                if self.calls[i].active
                    && self.calls[i].endpoint == index
                    && self.scheduler.task(thread).process != current
                {
                    self.fail_call(thread, Errno::Epipe);
                }
            }
            self.complete_receivers(index, false);
        }
        Ok(0)
    }

    pub(super) fn sys_endpoint_create(&mut self, current: ProcSlot) -> Result<u32, Errno> {
        create_endpoint(&mut self.ipc, &mut self.processes, current).map(Handle::raw)
    }

    pub(super) fn sys_endpoint_close(
        &mut self,
        current: ProcSlot,
        args: Args,
    ) -> Result<u32, Errno> {
        let handle = Handle(args.a0);
        let capability = self.processes.slot(current).capabilities.get(handle, 0)?;
        let ep = self.ipc.resolve(capability.object)?;
        if capability.rights & cap::CONTROL != 0 {
            for i in 0..TASKS {
                if self.calls[i].active && self.calls[i].endpoint == ep {
                    self.fail_call(ThreadSlot(i), Errno::Epipe);
                }
            }
            self.ipc.close(ep);
            self.invalidate_object(capability.object, None);
            self.complete_receivers(ep, true);
        } else {
            self.processes
                .slot_mut(current)
                .capabilities
                .close(handle)
                .unwrap();
            self.complete_receivers(ep, false);
        }
        Ok(0)
    }

    pub(super) fn sys_send(
        &mut self,
        current: ProcSlot,
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
        let ep = self.endpoint(current, Handle(args.a0), ipc::SEND)?;
        let mut message = Message::EMPTY;
        message.length = length;
        copy_from_user(self.memory(), args.a1 as usize, &mut message.data[..length])?;
        let ep = self.ipc.send(ep, message)?;
        self.complete_receivers(ep, false);
        self.scheduler.inherit();
        *switch = self.scheduler.should_preempt();
        Ok(0)
    }

    pub(super) fn sys_recv(
        &mut self,
        current: ProcSlot,
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
        let handle = Handle(args.a0);
        let ep = self.endpoint(current, handle, ipc::RECEIVE)?;
        match self.ipc.receive(ep, capacity)? {
            Some(message) => {
                self.bind_reply(message.reply_token, self.scheduler.current);
                frame.registers[9] = message.reply_token;
                copy_to_user_within(
                    self.memory(),
                    args.a1 as usize,
                    capacity,
                    &message.data[..message.length],
                )?;
                Ok(message.length as u32)
            }
            None if args.a3 & syscall::NONBLOCK != 0 => Err(Errno::Eagain),
            None => {
                // Check+wait registration+Blocked are one IRQ-masked
                // transaction, so send cannot race and lose the wakeup.
                self.scheduler.block(Wait::Receive {
                    endpoint: ep,
                    handle,
                    pointer: args.a1 as usize,
                    capacity,
                });
                *switch = true;
                if !self.logged_block {
                    self.logged_block = true;
                    let _ = writeln!(
                        self.console,
                        "[ipc] recv blocked task={}",
                        current.index() + 1
                    );
                }
                Ok(0) // replaced with completed length/error before wakeup
            }
        }
    }
}
