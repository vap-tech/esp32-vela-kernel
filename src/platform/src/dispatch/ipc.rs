use core::fmt::Write;
use vela_kernel::{
    errno::Errno,
    id::{Handle, Pid, ProcSlot},
    ipc::{self, Message},
    lifecycle,
    task::{Context, Wait},
};

use super::Args;
use crate::{
    arch,
    runtime::Runtime,
    syscall,
    usermem::{copy_between_users, copy_from_user, copy_to_user_within},
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
        let (ep, server) = lifecycle::find_rpc_server(
            &self.processes,
            &self.ipc,
            &self.scheduler,
            current,
            thread,
            Handle(args.a0),
        )?;
        let (generation, token) = self.calls.next_token(thread)?;
        let mut message = Message::EMPTY;
        message.length = length;
        message.reply_token = token;
        copy_from_user(self.memory(), args.a1 as usize, &mut message.data[..length])?;
        let until = if timeout == 0 {
            None
        } else {
            Some(self.scheduler.uptime().saturating_add(u64::from(timeout)))
        };
        let woken = lifecycle::commit_ipc_call(
            &self.processes,
            &mut self.ipc,
            &mut self.calls,
            &mut self.scheduler,
            ep,
            server,
            generation,
            message,
            args.a3 as usize,
            capacity,
            until,
            copy_to_user_within,
        )?;
        self.note_woken_receiver(woken);
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_ipc_reply(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let length = args.a2 as usize;
        let reply =
            lifecycle::prepare_ipc_reply(&self.calls, &self.scheduler, current, args.a0, length)?;
        copy_between_users(
            &self.processes.slot(current).memory,
            args.a1 as usize,
            &self.processes.slot(reply.target).memory,
            reply.pointer,
            reply.capacity,
            length,
        )?;
        lifecycle::finish_ipc_reply(&mut self.calls, &mut self.scheduler, reply.client, length);
        *switch = self.scheduler.should_preempt();
        Ok(0)
    }

    pub(super) fn sys_cap_or_mem_grant(
        &mut self,
        current: ProcSlot,
        args: Args,
    ) -> Result<u32, Errno> {
        let rights = u16::try_from(args.a2).map_err(|_| Errno::Einval)?;
        lifecycle::grant_or_map(
            &mut self.processes,
            current,
            Handle(args.a0),
            Pid(args.a1),
            rights,
            args.number == syscall::MEM_GRANT,
        )
        .map(Handle::raw)
    }

    pub(super) fn sys_cap_revoke(&mut self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        let woken = lifecycle::revoke_capability(
            &mut self.processes,
            &mut self.ipc,
            &mut self.calls,
            &mut self.scheduler,
            current,
            Handle(args.a0),
            copy_to_user_within,
        )?;
        self.note_woken_receiver(woken);
        Ok(0)
    }

    pub(super) fn sys_endpoint_create(&mut self, current: ProcSlot) -> Result<u32, Errno> {
        lifecycle::create_endpoint(&mut self.ipc, &mut self.processes, current).map(Handle::raw)
    }

    pub(super) fn sys_endpoint_close(
        &mut self,
        current: ProcSlot,
        args: Args,
    ) -> Result<u32, Errno> {
        let woken = lifecycle::close_endpoint(
            &mut self.processes,
            &mut self.ipc,
            &mut self.calls,
            &mut self.scheduler,
            current,
            Handle(args.a0),
            copy_to_user_within,
        )?;
        self.note_woken_receiver(woken);
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
                let server = self.scheduler.current;
                self.calls
                    .bind_reply(&mut self.scheduler, message.reply_token, server);
                frame.set_return2(message.reply_token);
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
                if !self.logs.block {
                    self.logs.block = true;
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
