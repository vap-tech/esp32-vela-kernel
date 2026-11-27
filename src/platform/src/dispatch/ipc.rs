use core::fmt::Write;
use vela_kernel::{
    call::{BULK_READ, BULK_WRITE, BulkWindow, CallExtra},
    capability::Capability,
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
    pub(super) fn sys_recovery_endpoint(
        &mut self,
        current: ProcSlot,
        args: Args,
    ) -> Result<u32, Errno> {
        use vela_kernel::{
            capability::SEND,
            process::{RECOVERY_KEY_BYTES, RECOVERY_SLOTS},
        };
        const _: () = assert!(
            RECOVERY_KEY_BYTES == syscall::SYSD_KEY_BYTES
                && RECOVERY_SLOTS == syscall::SYSD_RECOVERY_SLOTS
        );
        if self.sysd_pid != Some(self.processes.slot(current).id) {
            return Err(Errno::Eperm);
        }
        if args.number == syscall::SYSD_LOAD_ENDPOINT {
            if args.a0 as usize >= RECOVERY_SLOTS {
                return Err(Errno::Einval);
            }
            let Some(entry) = self.processes.recovery_entry(args.a0 as usize) else {
                return Ok(0);
            };
            self.ipc.resolve(entry.endpoint)?;
            copy_to_user_within(
                self.memory(),
                args.a1 as usize,
                RECOVERY_KEY_BYTES,
                &entry.key,
            )?;
            return self
                .processes
                .slot_mut(current)
                .capabilities
                .install(Capability {
                    object: entry.endpoint,
                    rights: SEND,
                })
                .map(Handle::raw);
        }
        let mut key = [0; RECOVERY_KEY_BYTES];
        let len = args.a2 as usize;
        if len == 0 || len > key.len() {
            return Err(Errno::Einval);
        }
        copy_from_user(self.memory(), args.a1 as usize, &mut key[..len])?;
        if key[..len].contains(&0) {
            return Err(Errno::Einval);
        }
        let object = if args.a0 == 0 {
            None
        } else {
            let ep = self.endpoint(current, Handle(args.a0), SEND)?;
            Some(self.ipc.object(ep))
        };
        self.processes.update_recovery(key, object)?;
        Ok(0)
    }

    #[allow(clippy::too_many_arguments)]
    fn do_ipc_call(
        &mut self,
        current: ProcSlot,
        endpoint_handle: Handle,
        req_ptr: usize,
        length: usize,
        reply_ptr: usize,
        capacity: usize,
        timeout: u32,
        bulk: Option<BulkWindow>,
        attached_cap: Option<Capability>,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let thread = self.scheduler.current;
        if length > ipc::MESSAGE_BYTES
            || capacity == 0
            || capacity > ipc::MESSAGE_BYTES
            || timeout > 6000
        {
            return Err(Errno::Einval);
        }
        if !self.readable(req_ptr, length) || !self.writable(reply_ptr, capacity) {
            return Err(Errno::Efault);
        }
        let (ep, server, caller_rights) = lifecycle::find_rpc_server_with_rights(
            &self.processes,
            &self.ipc,
            &self.scheduler,
            current,
            thread,
            endpoint_handle,
        )?;
        let (generation, token) = self.calls.next_token(thread)?;
        let bulk = bulk.map(|mut w| {
            if w.root_gen == 0 {
                w.root_thread = thread;
                w.root_gen = generation;
            }
            w
        });
        let extra = CallExtra {
            bulk,
            caller_rights,
            attached_cap,
        };
        let until = if timeout == 0 {
            None
        } else {
            Some(self.scheduler.uptime().saturating_add(u64::from(timeout)))
        };
        if let Some(woken_tid) = lifecycle::try_direct_ipc_call_extra(
            &self.processes,
            &self.ipc,
            &mut self.calls,
            &mut self.scheduler,
            current,
            ep,
            generation,
            token,
            req_ptr,
            length,
            reply_ptr,
            capacity,
            until,
            extra,
            copy_between_users,
        ) {
            self.note_woken_receiver(Some(woken_tid));
            *switch = true;
            return Ok(0);
        }
        let mut message = Message::EMPTY;
        message.length = length;
        message.reply_token = token;
        copy_from_user(self.memory(), req_ptr, &mut message.data[..length])?;
        let woken = lifecycle::commit_ipc_call_extra(
            &self.processes,
            &mut self.ipc,
            &mut self.calls,
            &mut self.scheduler,
            ep,
            server,
            generation,
            message,
            reply_ptr,
            capacity,
            until,
            extra,
            copy_to_user_within,
        )?;
        self.note_woken_receiver(woken);
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_ipc_call(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        self.do_ipc_call(
            current,
            Handle(args.a0),
            args.a1 as usize,
            args.a2 as usize,
            args.a3 as usize,
            args.a4 as usize,
            args.a5,
            None,
            None,
            switch,
        )
    }

    pub(super) fn sys_ipc_call_bulk(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let mut raw = [0u8; core::mem::size_of::<syscall::IpcBulkCall>()];
        copy_from_user(self.memory(), args.a1 as usize, &mut raw)?;
        let word =
            |off: usize| u32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
        let req_ptr = word(0) as usize;
        let req_len = word(4) as usize;
        let reply_ptr = word(8) as usize;
        let reply_cap = word(12) as usize;
        let timeout = word(16);
        let bulk_ptr = word(20) as usize;
        let bulk_len = word(24) as usize;
        let bulk_flags = word(28);
        let forward_token = word(32);

        let allowed = syscall::IPC_BULK_READ | syscall::IPC_BULK_WRITE | syscall::IPC_BULK_FORWARD;
        if bulk_flags == 0 || (bulk_flags & !allowed) != 0 {
            return Err(Errno::Einval);
        }
        let bulk = if (bulk_flags & syscall::IPC_BULK_FORWARD) != 0 {
            let mut window = self
                .calls
                .lookup_bulk(forward_token, self.scheduler.current)?;
            let req_rw = (bulk_flags & (syscall::IPC_BULK_READ | syscall::IPC_BULK_WRITE)) as u8;
            if req_rw != 0 {
                if (req_rw & !window.flags) != 0 {
                    return Err(Errno::Eperm);
                }
                window.flags = req_rw;
            }
            Some(window)
        } else {
            let rw = (bulk_flags & (syscall::IPC_BULK_READ | syscall::IPC_BULK_WRITE)) as u8;
            if rw == 0 || bulk_len == 0 || bulk_len > syscall::IPC_BULK_MAX {
                return Err(Errno::Einval);
            }
            if (rw & BULK_READ != 0 && !self.readable(bulk_ptr, bulk_len))
                || (rw & BULK_WRITE != 0 && !self.writable(bulk_ptr, bulk_len))
            {
                return Err(Errno::Efault);
            }
            Some(BulkWindow {
                root_thread: self.scheduler.current,
                root_gen: 0, // filled in do_ipc_call once generation is allocated
                owner: current,
                ptr: bulk_ptr,
                len: bulk_len,
                flags: rw,
            })
        };
        self.do_ipc_call(
            current,
            Handle(args.a0),
            req_ptr,
            req_len,
            reply_ptr,
            reply_cap,
            timeout,
            bulk,
            None,
            switch,
        )
    }

    pub(super) fn sys_ipc_bulk_io(
        &mut self,
        current: ProcSlot,
        args: Args,
        frame: &mut arch::TrapFrame,
    ) -> Result<u32, Errno> {
        let token = args.a0;
        let offset = args.a1 as usize;
        let local_ptr = args.a2 as usize;
        let len = args.a3 as usize;
        let dir = args.a4;
        let server = self.scheduler.current;
        if local_ptr == 0 && len == 0 {
            let bulk = self.calls.lookup_bulk(token, server)?;
            frame.set_return2(u32::from(bulk.flags));
            return Ok(bulk.len as u32);
        }
        let write_to_client = match dir {
            syscall::IPC_BULK_DIR_READ => false,
            syscall::IPC_BULK_DIR_WRITE => true,
            _ => return Err(Errno::Einval),
        };
        let (client_proc, client_addr) =
            self.calls
                .prepare_bulk_io(token, server, offset, len, write_to_client)?;
        let started = unsafe { crate::soc::timer::counter_16mhz() }.ok_or(Errno::Eio)?;
        if write_to_client {
            copy_between_users(
                &self.processes.slot(current).memory,
                local_ptr,
                &self.processes.slot(client_proc).memory,
                client_addr,
                len,
                len,
            )?;
        } else {
            copy_between_users(
                &self.processes.slot(client_proc).memory,
                client_addr,
                &self.processes.slot(current).memory,
                local_ptr,
                len,
                len,
            )?;
        }
        self.read_profile.bulk_copy_ticks +=
            unsafe { crate::soc::timer::counter_16mhz() }.ok_or(Errno::Eio)? - started;
        self.read_profile.bulk_calls += 1;
        Ok(len as u32)
    }

    pub(super) fn sys_ipc_call_cap(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let mut raw = [0u8; core::mem::size_of::<syscall::IpcCapCall>()];
        copy_from_user(self.memory(), args.a1 as usize, &mut raw)?;
        let word =
            |off: usize| u32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]);
        let req_ptr = word(0) as usize;
        let req_len = word(4) as usize;
        let reply_ptr = word(8) as usize;
        let reply_cap = word(12) as usize;
        let timeout = word(16);
        let cap_handle = word(20);
        let cap_rights = word(24);
        let attached_cap = if cap_handle == 0 {
            None
        } else {
            let rights = u16::try_from(cap_rights).map_err(|_| Errno::Einval)?;
            Some(lifecycle::prepare_ipc_cap(
                &self.processes,
                &self.ipc,
                current,
                Handle(cap_handle),
                rights,
            )?)
        };
        self.do_ipc_call(
            current,
            Handle(args.a0),
            req_ptr,
            req_len,
            reply_ptr,
            reply_cap,
            timeout,
            None,
            attached_cap,
            switch,
        )
    }

    pub(super) fn sys_ipc_accept_cap(
        &mut self,
        current: ProcSlot,
        args: Args,
        frame: &mut arch::TrapFrame,
    ) -> Result<u32, Errno> {
        let token = args.a0;
        let flags = args.a1;
        let server = self.scheduler.current;
        match flags {
            0 => {
                let (handle, caller_rights) = lifecycle::accept_ipc_cap(
                    &mut self.processes,
                    &self.ipc,
                    &mut self.calls,
                    current,
                    server,
                    token,
                )?;
                frame.set_return2(u32::from(caller_rights));
                Ok(handle.raw())
            }
            syscall::IPC_ACCEPT_CAP_QUERY => {
                let (caller_rights, has_cap) = self.calls.query_meta(token, server)?;
                frame.set_return2(u32::from(has_cap));
                Ok(u32::from(caller_rights))
            }
            _ => Err(Errno::Einval),
        }
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

    pub(super) fn sys_ipc_reply_recv(
        &mut self,
        current: ProcSlot,
        args: Args,
        frame: &mut arch::TrapFrame,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let reply_token = args.a0;
        let reply_ptr = args.a1 as usize;
        let reply_len = args.a2 as usize;
        let recv_handle = Handle(args.a3);
        let recv_ptr = args.a4 as usize;
        let recv_cap = args.a5 as usize;

        if recv_cap == 0 || recv_cap > ipc::MESSAGE_BYTES {
            return Err(Errno::Einval);
        }
        if !self.writable(recv_ptr, recv_cap) {
            return Err(Errno::Efault);
        }
        let ep = self.endpoint(current, recv_handle, ipc::RECEIVE)?;

        if reply_token != 0 {
            if reply_len > ipc::MESSAGE_BYTES {
                return Err(Errno::Emsgsize);
            }
            if !self.readable(reply_ptr, reply_len) {
                return Err(Errno::Efault);
            }
            match lifecycle::prepare_ipc_reply(
                &self.calls,
                &self.scheduler,
                current,
                reply_token,
                reply_len,
            ) {
                Ok(reply) => {
                    if copy_between_users(
                        &self.processes.slot(current).memory,
                        reply_ptr,
                        &self.processes.slot(reply.target).memory,
                        reply.pointer,
                        reply.capacity,
                        reply_len,
                    )
                    .is_ok()
                    {
                        lifecycle::finish_ipc_reply(
                            &mut self.calls,
                            &mut self.scheduler,
                            reply.client,
                            reply_len,
                        );
                    } else {
                        self.calls.deactivate(reply.client);
                        self.scheduler
                            .wake_with(reply.client, Errno::Efault.to_abi());
                        self.scheduler.inherit();
                        self.scheduler.prefer(reply.client);
                    }
                }
                // Timed-out/exited client or undersized client buffer: keep the server loop alive.
                Err(Errno::Epipe | Errno::Emsgsize) => {}
                Err(e) => return Err(e),
            }
        }

        match self.ipc.receive(ep, recv_cap)? {
            Some(message) => {
                let server = self.scheduler.current;
                self.calls
                    .bind_reply(&mut self.scheduler, message.reply_token, server);
                frame.set_return2(message.reply_token);
                copy_to_user_within(
                    self.memory(),
                    recv_ptr,
                    recv_cap,
                    &message.data[..message.length],
                )?;
                *switch = self.scheduler.should_preempt();
                Ok(message.length as u32)
            }
            None => {
                self.scheduler.block(Wait::Receive {
                    endpoint: ep,
                    handle: recv_handle,
                    pointer: recv_ptr,
                    capacity: recv_cap,
                });
                *switch = true;
                Ok(0)
            }
        }
    }

    pub(super) fn sys_cap_or_mem_grant(
        &mut self,
        current: ProcSlot,
        args: Args,
    ) -> Result<u32, Errno> {
        let rights = u16::try_from(args.a2).map_err(|_| Errno::Einval)?;
        let granted = lifecycle::grant_or_map(
            &mut self.processes,
            current,
            Handle(args.a0),
            Pid(args.a1),
            rights,
            args.number == syscall::MEM_GRANT,
        )?;
        if rights & vela_kernel::capability::SEND != 0
            && let Ok(src) = self
                .processes
                .slot(current)
                .capabilities
                .get(Handle(args.a0), 0)
            && let Ok(ep) = self.ipc.resolve(src.object)
        {
            self.ipc.mark_sender_attached(ep);
        }
        Ok(granted.raw())
    }

    pub(super) fn sys_cap_revoke(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let revoked = lifecycle::revoke_capability(
            &mut self.processes,
            &mut self.ipc,
            &mut self.calls,
            &mut self.events,
            &mut self.scheduler,
            current,
            Handle(args.a0),
            copy_to_user_within,
        )?;
        self.shootdown_stale_remote_pmp();
        if let Some(index) = revoked.mask_event {
            self.mask_irq_slot(index);
        }
        self.note_woken_receiver(revoked.first_woken);
        *switch = self.scheduler.should_preempt();
        Ok(0)
    }

    pub(super) fn sys_endpoint_create(
        &mut self,
        current: ProcSlot,
        args: Args,
    ) -> Result<u32, Errno> {
        if args.a0 & !syscall::ENDPOINT_STREAM != 0 {
            return Err(Errno::Einval);
        }
        lifecycle::create_endpoint_with_flags(
            &mut self.ipc,
            &mut self.processes,
            current,
            (args.a0 & syscall::ENDPOINT_STREAM) != 0,
        )
        .map(Handle::raw)
    }

    pub(super) fn sys_endpoint_close(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
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
        *switch = self.scheduler.should_preempt();
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
