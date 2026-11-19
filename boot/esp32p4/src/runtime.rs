use core::{cell::UnsafeCell, fmt::Write, mem::MaybeUninit};
use vela_kernel::{
    capability::{self as cap, Capability, Object},
    ipc::{self, Ipc, Message},
    memory::RegionAllocator,
    process::{AddressSpace, EXECUTE, Lifecycle, Processes, READ, WRITE},
    sched::{Scheduler, TASKS},
    sentinel::{WakePhase, WakeTicket},
    task::{Region, State, Wait},
};

use crate::{arch, pmp, sentinel_wire, soc, syscall};

#[derive(Clone, Copy)]
pub struct Call {
    pub generation: u32,
    pub active: bool,
    pub endpoint: usize,
    pub server: Option<usize>,
}

impl Call {
    pub const EMPTY: Self = Self {
        generation: 0,
        active: false,
        endpoint: 0,
        server: None,
    };
}

pub struct Runtime {
    pub scheduler: Scheduler<arch::TrapFrame>,
    pub processes: Processes,
    pub active_space: Option<(usize, u32)>,
    pub pmp_images: [Option<(u32, pmp::Prepared)>; vela_kernel::process::PROCESSES],
    pub allocator: RegionAllocator,
    pub console: soc::console::Console,
    pub ipc: Ipc,
    pub calls: [Call; TASKS],
    pub idle_frame: arch::TrapFrame,
    pub in_idle: bool,
    pub dma: vela_kernel::dma::Lease,
    pub dma_length: usize,
    pub dma_deadline: u64,
    pub sentinel_epoch: Option<u32>,
    pub sentinel_ready: bool,
    pub sentinel_wake: Option<WakeTicket>,
    pub sentinel_sequence: u32,
    pub sentinel_pending: bool,
    pub sentinel_command: u32,
    pub sentinel_argument: u32,
    pub sentinel_diagnostics: u32,
    pub sentinel_hz: u32,
    pub heartbeat_link: vela_kernel::sentinel::Heartbeat,
    pub display_mode: u8,
    pub display_stage: u8,
    pub sentinel_heartbeat: bool,
    pub sentinel_next_beat: u64,
    pub sentinel_notifications: u32,
    pub sentinel_last_publish: u32,
    pub usb_rx_interrupts: u64,
    pub logged_block: bool,
    pub logged_wake: bool,
    pub logged_idle: bool,
}

pub const CONSOLE_RX: Object = Object::Event {
    index: 0,
    generation: 1,
};

pub struct BootCell(pub UnsafeCell<MaybeUninit<Runtime>>);
// SAFETY: hart0 only, HP1 reset, MIE=0 in every Rust kernel path. The M-mode
// WFI loop has no Rust references and never accesses this cell. Initialization
// precedes timer enable; outermost traps are the only later borrowers.
unsafe impl Sync for BootCell {}
pub static RUNTIME: BootCell = BootCell(UnsafeCell::new(MaybeUninit::uninit()));

/// Kernel copy only, caller validated a writable task-local stack range. No user
/// executes during this copy and no Rust reference aliases the user buffer.
pub unsafe fn deliver(pointer: usize, message: &Message) {
    for i in 0..message.length {
        unsafe {
            (pointer as *mut u8).add(i).write(message.data[i]);
        }
    }
}

pub fn create_endpoint(ipc: &mut Ipc, processes: &mut Processes, owner: usize) -> Result<u32, u32> {
    let object = ipc.create(owner)?;
    match processes.slots[owner].capabilities.install(Capability {
        object,
        rights: cap::SEND | cap::RECEIVE | cap::GRANT | cap::CONTROL,
    }) {
        Ok(handle) => Ok(handle),
        Err(e) => {
            let ep = ipc.resolve(object)?;
            ipc.close(ep);
            Err(e)
        }
    }
}

pub fn grant(
    processes: &mut Processes,
    owner: usize,
    handle: u32,
    peer: usize,
    rights: u16,
) -> Result<u32, u32> {
    let source = processes.slots[owner]
        .capabilities
        .get(handle, cap::GRANT)?;
    if rights == 0 || rights & !source.rights != 0 || rights & (cap::CONTROL | cap::GRANT) != 0 {
        return Err(cap::EPERM);
    }
    processes.slots[peer].capabilities.install(Capability {
        object: source.object,
        rights,
    })
}

impl Runtime {
    pub fn finish_wake_waiter(&mut self, ticket: WakeTicket, result: u32, elapsed: u32) -> bool {
        if let Some(i) = self.scheduler.tasks.iter().position(|t| {
            t.state == State::Blocked
                && self.processes.slots[t.process].id == ticket.owner
                && matches!(t.wait, Wait::SentinelWake { token, .. } if token == ticket.token)
        }) {
            self.scheduler.tasks[i].context.registers[8] = result;
            self.scheduler.tasks[i].context.registers[9] = elapsed;
            self.scheduler.wake(i);
            return true;
        }
        false
    }

    pub fn receive_wake(&mut self, reply: sentinel_wire::Reply) {
        let Some(mut ticket) = self.sentinel_wake else {
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
        let now = unsafe { soc::timer::uptime_ticks() }.expect("SYSTIMER wake snapshot");
        let elapsed = now.saturating_sub(ticket.started).min(u64::from(u32::MAX)) as u32;
        ticket.phase = WakePhase::Fired(elapsed);
        self.sentinel_wake = Some(ticket);
        let alive = self
            .processes
            .slots
            .iter()
            .any(|p| p.id == ticket.owner && p.state == Lifecycle::Alive);
        if !alive || self.finish_wake_waiter(ticket, ticket.token, elapsed) {
            self.sentinel_wake = None;
        }
        // Otherwise retain the event until WAIT. An IRQ between ARM ACK and
        // userspace WAIT must not turn into a lost wakeup.
    }

    pub fn sentinel_request(&mut self, command: u32, arg0: u32, arg1: u32) -> Result<u32, u32> {
        if !self.sentinel_ready {
            return Err(syscall::EAGAIN);
        }
        if self.heartbeat_link.offline {
            return Err((-110i32) as u32);
        }
        if self.sentinel_pending {
            return Err((-16i32) as u32);
        }
        if self.sentinel_sequence == i32::MAX as u32 {
            return Err((-75i32) as u32);
        }
        self.sentinel_sequence += 1;
        self.sentinel_pending = true;
        self.sentinel_command = command;
        self.sentinel_argument = arg0;
        // SAFETY: hart0 MIE=0, only one pending request until its exact response.
        unsafe {
            soc::sentinel::request(self.sentinel_sequence, command, arg0, arg1);
        }
        Ok(self.sentinel_sequence)
    }

    pub fn sentinel_reply(&mut self, reply: sentinel_wire::Reply) {
        if reply.published <= self.sentinel_last_publish {
            return;
        }
        self.sentinel_last_publish = reply.published;
        match reply.event {
            sentinel_wire::READY if reply.result == sentinel_wire::OK => {
                self.sentinel_ready = true;
                return;
            }
            sentinel_wire::HEARTBEAT_MISSED
                if reply.request == 0 && reply.result == sentinel_wire::OK =>
            {
                self.sentinel_notifications = self.sentinel_notifications.saturating_add(1);
                return;
            }
            sentinel_wire::DIAGNOSTIC_EVENT
                if reply.request == 0
                    && reply.result != 0
                    && (reply.age as u32 >> 16) == u32::from(sentinel_wire::LOG_HP_EVENT) =>
            {
                self.sentinel_diagnostics = self.sentinel_diagnostics.saturating_add(1);
                return;
            }
            sentinel_wire::WAKE_EVENT
                if reply.request == 0 && reply.result == sentinel_wire::OK =>
            {
                self.receive_wake(reply);
                return;
            }
            _ => (),
        }
        if !self.sentinel_pending || reply.request != self.sentinel_sequence {
            return;
        }
        let command = self.sentinel_command;
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
        self.sentinel_pending = false;
        if command == sentinel_wire::HEARTBEAT {
            self.heartbeat_link.acknowledge(reply.request);
        }
        let error_code = match reply.result {
            sentinel_wire::LOG_GONE | sentinel_wire::LOG_EXHAUSTED => (-75i32) as u32,
            sentinel_wire::LOG_UNKNOWN => (-2i32) as u32,
            sentinel_wire::LOG_BUSY => (-16i32) as u32,
            sentinel_wire::WAKE_ALREADY => (-114i32) as u32,
            _ => syscall::EINVAL,
        };
        if matches!(
            command,
            sentinel_wire::SET_WAKEUP | sentinel_wire::PREPARE_SLEEP
        ) {
            if let Some(mut ticket) = self.sentinel_wake {
                if ticket.token == reply.request {
                    if error {
                        self.finish_wake_waiter(ticket, error_code, 0);
                        self.sentinel_wake = None;
                    } else if matches!(ticket.phase, WakePhase::Requested) {
                        ticket.phase = WakePhase::Armed;
                        self.sentinel_wake = Some(ticket);
                    }
                }
            }
        } else if matches!(
            command,
            sentinel_wire::CANCEL_WAKEUP | sentinel_wire::CANCEL_SLEEP
        ) && !error
        {
            if let Some(ticket) = self.sentinel_wake {
                if ticket.token == self.sentinel_argument {
                    self.finish_wake_waiter(ticket, (-125i32) as u32, 0);
                    self.sentinel_wake = None;
                }
            }
        }
        let Some(i) = self.scheduler.tasks.iter().position(|t| {
            t.state == State::Blocked
                && matches!(t.wait, Wait::Sentinel { sequence, .. } if sequence == reply.request)
        }) else {
            return;
        };
        let Wait::Sentinel { pointer, .. } = self.scheduler.tasks[i].wait else {
            unreachable!()
        };
        let result = if error {
            error_code
        } else if pointer == 0 {
            reply.request
        } else {
            let process = self.scheduler.tasks[i].process;
            if !self.processes.slots[process].memory.permits(
                pointer,
                sentinel_wire::LOG_BYTES,
                WRITE,
            ) {
                syscall::EFAULT
            } else {
                let values = match command {
                    sentinel_wire::GET_STATUS => {
                        let age_ms = reply.age * 1000 / u64::from(self.sentinel_hz);
                        [
                            reply.monitor & 3,
                            age_ms as u32,
                            (age_ms >> 32) as u32,
                            self.sentinel_hz,
                            reply.beats,
                            reply.monitor >> 2,
                        ]
                    }
                    sentinel_wire::LOG_META => [
                        self.sentinel_epoch.unwrap_or(0),
                        reply.monitor,
                        reply.age as u32,
                        (reply.age >> 32) as u32,
                        reply.beats,
                        self.sentinel_hz,
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
                if command == sentinel_wire::LOG_READ && values[0] != self.sentinel_argument {
                    self.scheduler.tasks[i].context.registers[8] = syscall::EINVAL;
                    self.scheduler.wake(i);
                    return;
                }
                for (index, value) in values.into_iter().enumerate() {
                    for (byte_index, byte) in value.to_le_bytes().into_iter().enumerate() {
                        // SAFETY: full buffer revalidated against receiver's current process mappings.
                        unsafe {
                            (pointer as *mut u8).add(index * 4 + byte_index).write(byte);
                        }
                    }
                }
                sentinel_wire::LOG_BYTES as u32
            }
        };
        self.scheduler.tasks[i].context.registers[8] = result;
        self.scheduler.tasks[i].context.registers[9] = if command == sentinel_wire::LOG_META {
            self.sentinel_diagnostics
        } else {
            self.sentinel_notifications
        };
        self.scheduler.wake(i);
    }

    pub fn sentinel_tick(&mut self, uptime: u64) {
        if self.heartbeat_link.retry_due(uptime) {
            // Keep the slot and payload stable until the original heartbeat completes.
            unsafe {
                soc::sentinel::retry_heartbeat(self.sentinel_sequence);
            }
            return;
        }
        if self.sentinel_ready
            && self.sentinel_heartbeat
            && !self
                .sentinel_wake
                .is_some_and(|t| t.sleep && !matches!(t.phase, WakePhase::Fired(_)))
            && !self.sentinel_pending
            && uptime >= self.sentinel_next_beat
            && self
                .sentinel_request(sentinel_wire::HEARTBEAT, 3000, 0)
                .is_ok()
        {
            self.heartbeat_link.sent(self.sentinel_sequence, uptime);
            self.sentinel_next_beat = uptime.saturating_add(100);
        }
    }

    pub fn bind_reply(&mut self, token: u32, server: usize) {
        if token == 0 {
            return;
        }
        let client = (token & 255).wrapping_sub(1) as usize;
        if let Some(call) = self.calls.get_mut(client) {
            if call.active && call.generation == token >> 8 {
                call.server = Some(server);
                if let Wait::Reply {
                    token,
                    pointer,
                    capacity,
                    until,
                    ..
                } = self.scheduler.tasks[client].wait
                {
                    self.scheduler.tasks[client].wait = Wait::Reply {
                        token,
                        pointer,
                        capacity,
                        until,
                        server,
                    };
                }
            }
        }
    }

    pub fn fail_call(&mut self, client: usize, error: u32) {
        self.calls[client].active = false;
        if matches!(self.scheduler.tasks[client].wait, Wait::Reply { .. }) {
            self.scheduler.tasks[client].context.registers[8] = error;
            self.scheduler.wake(client);
        }
    }

    pub fn expire_calls(&mut self, uptime: u64) {
        for i in 0..TASKS {
            if self.calls[i].active {
                if let Wait::Reply { until, .. } = self.scheduler.tasks[i].wait {
                    if until != 0 && until <= uptime {
                        self.fail_call(i, (-110i32) as u32);
                    }
                }
            }
        }
    }

    pub fn endpoint(&self, process: usize, handle: u32, rights: u16) -> Result<usize, u32> {
        let capability = self.processes.slots[process]
            .capabilities
            .get(handle, rights)?;
        self.ipc.resolve(capability.object)
    }

    pub fn invalidate_object(&mut self, object: Object, keep_owner: Option<usize>) {
        for process in 0..self.processes.slots.len() {
            if Some(process) == keep_owner {
                continue;
            }
            self.processes.slots[process].capabilities.revoke(object);
            if let Object::Memory { region, .. } = object {
                if self.processes.slots[process]
                    .memory
                    .maps
                    .iter()
                    .any(|m| m.region == region)
                {
                    self.processes.slots[process].memory.remove(region).unwrap();
                    for thread in 0..TASKS {
                        if self.scheduler.tasks[thread].process == process {
                            if let Wait::Receive {
                                pointer, capacity, ..
                            } = self.scheduler.tasks[thread].wait
                            {
                                if region.contains(pointer, capacity) {
                                    self.scheduler.tasks[thread].context.registers[8] =
                                        syscall::EFAULT;
                                    self.scheduler.wake(thread);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn memory(&self) -> &AddressSpace {
        &self.processes.slots[self.scheduler.current().process].memory
    }

    pub fn readable(&self, pointer: usize, length: usize) -> bool {
        self.memory().permits(pointer, length, READ)
    }

    pub fn writable(&self, pointer: usize, length: usize) -> bool {
        self.memory().permits(pointer, length, WRITE)
    }

    pub fn complete_receivers(&mut self, endpoint: usize, closed: bool) {
        for i in 0..TASKS {
            let task = self.scheduler.tasks[i];
            if task.state != State::Blocked {
                continue;
            }
            if let Wait::Receive {
                endpoint: waiting,
                handle,
                pointer,
                capacity,
            } = task.wait
            {
                if waiting != endpoint {
                    continue;
                }
                let result = if closed || self.endpoint(task.process, handle, ipc::RECEIVE).is_err()
                {
                    Err(ipc::EPIPE)
                } else {
                    self.ipc.receive(endpoint, capacity)
                };
                let value = match result {
                    Ok(None) => continue,
                    Ok(Some(message)) => {
                        self.bind_reply(message.reply_token, i);
                        self.scheduler.tasks[i].context.registers[9] = message.reply_token;
                        if !self.processes.slots[task.process]
                            .memory
                            .permits(pointer, capacity, WRITE)
                        {
                            syscall::EFAULT
                        } else {
                            // SAFETY: full writable range revalidated against receiver's mapping.
                            unsafe {
                                deliver(pointer, &message);
                            }
                            message.length as u32
                        }
                    }
                    Err(error) => error,
                };
                // Blocked ecall already advanced mepc and was saved. Update its
                // own return register before making the task runnable.
                self.scheduler.tasks[i].context.registers[8] = value;
                self.scheduler.wake(i);
                if let Wait::Receive { .. } = task.wait {
                    self.scheduler.prefer(i);
                }
                if !self.logged_wake {
                    self.logged_wake = true;
                    let _ = writeln!(self.console, "[ipc] receiver woke task={}", task.id);
                }
            }
        }
    }

    pub fn terminate(&mut self) {
        let process = self.scheduler.current().process;
        let exiting = self.scheduler.current;
        self.calls[exiting].active = false;
        for i in 0..TASKS {
            if self.calls[i].active && self.calls[i].server == Some(exiting) {
                self.fail_call(i, ipc::EPIPE);
            }
        }
        self.scheduler.terminate();
        if self
            .scheduler
            .tasks
            .iter()
            .any(|t| t.process == process && t.state != State::Terminated)
        {
            return;
        }
        let owner = self.processes.slots[process].id;
        if self
            .sentinel_wake
            .is_some_and(|ticket| ticket.release_on_exit(owner))
        {
            self.sentinel_wake = None;
        }
        self.dma.release_process(owner);
        self.processes.slots[process].state = Lifecycle::Dying;
        let objects: [Object; ipc::MAX_ENDPOINTS] = core::array::from_fn(|i| self.ipc.object(i));
        for i in 0..TASKS {
            if self.calls[i].active && self.ipc.owner(self.calls[i].endpoint) == Some(process) {
                self.fail_call(i, ipc::EPIPE);
            }
        }
        let closed = self.ipc.remove_task(process);
        for (i, is_closed) in closed.iter().enumerate() {
            if *is_closed {
                self.invalidate_object(objects[i], None);
            }
        }
        for capability in self.processes.slots[process]
            .capabilities
            .objects()
            .into_iter()
            .flatten()
        {
            if let Object::Memory { owner, .. } = capability.object {
                if owner == self.processes.slots[process].id {
                    self.invalidate_object(capability.object, Some(process));
                }
            }
        }
        self.processes.slots[process].capabilities.clear();
        self.allocator
            .release_owner(self.processes.slots[process].id);
        self.processes.slots[process].state = Lifecycle::Free;
        self.processes.slots[process].memory = AddressSpace::EMPTY;
        self.pmp_images[process] = None;
        for (ep, is_closed) in closed.into_iter().enumerate() {
            if is_closed {
                self.complete_receivers(ep, true);
            }
        }
    }

    pub fn spawn_elf(&mut self, bytes: &[u8], dma_driver: bool) -> Result<u32, u32> {
        let image = vela_kernel::elf::Image::parse(bytes).map_err(|_| 8u32)?;
        let process = self.processes.create(AddressSpace::EMPTY)?;
        let owner = self.processes.slots[process].id;
        let result = (|| {
            let mut space = AddressSpace::EMPTY;
            let bias = if image.pic {
                let end = image
                    .segments()
                    .iter()
                    .map(|s| s.region.base + s.region.size)
                    .max()
                    .ok_or(8u32)?;
                self.allocator.allocate(owner, end)?.base
            } else {
                0
            };
            for (i, segment) in image.segments().iter().enumerate() {
                let region = if image.pic {
                    let size = segment
                        .region
                        .size
                        .max(128)
                        .checked_next_power_of_two()
                        .ok_or(8u32)?;
                    if segment.region.base % size != 0 {
                        return Err(8);
                    }
                    Region {
                        base: bias + segment.region.base,
                        size,
                    }
                } else {
                    self.allocator
                        .reserve(owner, segment.region.base, segment.region.size)?
                };
                let mut rights = READ;
                if segment.flags & 1 != 0 {
                    rights |= EXECUTE;
                }
                if segment.flags & 2 != 0 {
                    rights |= WRITE;
                }
                space.insert(region, rights)?;
                let data = image.data(i).ok_or(8u32)?;
                // Reserved private SRAM. Clear BSS and all PMP-visible padding.
                unsafe {
                    core::ptr::write_bytes(region.base as *mut u8, 0, region.size);
                    core::ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        region.base as *mut u8,
                        data.len(),
                    );
                }
            }
            image
                .visit_relocations(|target, addend| {
                    // Parser validated the destination RW word and image-relative addend.
                    unsafe {
                        ((bias + target) as *mut u32).write((bias + addend) as u32);
                    }
                })
                .map_err(|_| 8u32)?;
            let entry = bias + image.entry;
            let stack = self.allocator.allocate(owner, 4096)?;
            space.insert(stack, READ | WRITE)?;
            let arg = if dma_driver {
                if !self.dma.acquire(owner) {
                    return Err(16);
                }
                let page = soc::dma::page();
                space.insert(page, READ | WRITE)?;
                unsafe {
                    core::ptr::write_bytes(page.base as *mut u8, 0, page.size);
                }
                page.base as u32
            } else {
                0
            };
            let prepared = pmp::Prepared::new(&space)?;
            unsafe {
                core::ptr::write_bytes(stack.base as *mut u8, 0, stack.size);
                core::arch::asm!("fence.i", options(nostack));
            }
            let frame = arch::TrapFrame::user(entry, stack.base + stack.size, arg);
            self.scheduler
                .create_thread(process, frame, stack, entry, 1, 1)?;
            self.processes.slots[process].memory = space;
            self.pmp_images[process] = Some((space.revision, prepared));
            Ok(owner)
        })();
        if result.is_err() {
            self.dma.release_process(owner);
            self.allocator.release_owner(owner);
            self.processes.slots[process] = vela_kernel::process::Process::EMPTY;
        }
        result
    }

    pub fn complete_dma(&mut self, success: bool) {
        let Some(owner) = self.dma.finish(success) else {
            return;
        };
        let Some(process) = self
            .processes
            .slots
            .iter()
            .position(|p| p.id == owner && p.state == Lifecycle::Alive)
        else {
            return;
        };
        if success {
            self.processes.slots[process]
                .memory
                .dma_access(soc::dma::page(), true)
                .unwrap();
        }
        for i in 0..TASKS {
            let t = &mut self.scheduler.tasks[i];
            if t.process == process && t.state == State::Blocked && matches!(t.wait, Wait::Dma) {
                t.context.registers[8] = if success { 0 } else { (-5i32) as u32 };
                self.scheduler.wake(i);
            }
        }
    }
}
