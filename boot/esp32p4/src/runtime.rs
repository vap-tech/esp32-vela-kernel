use core::{cell::UnsafeCell, fmt::Write, mem::MaybeUninit};
use vela_kernel::{
    capability::{self as cap, Capability, Object},
    errno::Errno,
    id::{Handle, Pid, ProcSlot, ThreadSlot},
    ipc::{self, Ipc},
    memory::RegionAllocator,
    process::{AddressSpace, EXECUTE, Lifecycle, Processes, READ, WRITE},
    sched::{Scheduler, TASKS},
    task::{Region, State, Wait},
};

use crate::{
    arch, calls::CallTable, display::DisplayControl, dma::DmaChannel, pmp,
    sentinel::SentinelClient, soc, usermem::copy_to_user_within,
};

pub struct BootLogs {
    pub block: bool,
    pub wake: bool,
    pub idle: bool,
}

impl BootLogs {
    pub const EMPTY: Self = Self {
        block: false,
        wake: false,
        idle: false,
    };
}

pub struct Runtime {
    pub scheduler: Scheduler<arch::TrapFrame>,
    pub processes: Processes,
    pub active_space: Option<(ProcSlot, u32)>,
    pub pmp_images: [Option<(u32, pmp::Prepared)>; vela_kernel::process::PROCESSES],
    pub allocator: RegionAllocator,
    pub console: soc::console::Console,
    pub ipc: Ipc,
    pub calls: CallTable,
    pub idle_frame: arch::TrapFrame,
    pub in_idle: bool,
    pub dma: DmaChannel,
    pub sentinel: SentinelClient,
    pub display: DisplayControl,
    pub usb_rx_interrupts: u64,
    pub logs: BootLogs,
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

pub fn create_endpoint(
    ipc: &mut Ipc,
    processes: &mut Processes,
    owner: ProcSlot,
) -> Result<Handle, Errno> {
    let object = ipc.create(owner)?;
    match processes.slot_mut(owner).capabilities.install(Capability {
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
    owner: ProcSlot,
    handle: Handle,
    peer: ProcSlot,
    rights: u16,
) -> Result<Handle, Errno> {
    let source = processes.slot(owner).capabilities.get(handle, cap::GRANT)?;
    if rights == 0 || rights & !source.rights != 0 || rights & (cap::CONTROL | cap::GRANT) != 0 {
        return Err(Errno::Eperm);
    }
    processes.slot_mut(peer).capabilities.install(Capability {
        object: source.object,
        rights,
    })
}

impl Runtime {
    pub fn endpoint(&self, process: ProcSlot, handle: Handle, rights: u16) -> Result<usize, Errno> {
        let capability = self
            .processes
            .slot(process)
            .capabilities
            .get(handle, rights)?;
        self.ipc.resolve(capability.object)
    }

    pub fn invalidate_object(&mut self, object: Object, keep_owner: Option<ProcSlot>) {
        for idx in 0..self.processes.slots.len() {
            let process = ProcSlot(idx);
            if Some(process) == keep_owner {
                continue;
            }
            self.processes.slot_mut(process).capabilities.revoke(object);
            if let Object::Memory { region, .. } = object {
                if self
                    .processes
                    .slot(process)
                    .memory
                    .maps
                    .iter()
                    .any(|m| m.region == region)
                {
                    self.processes
                        .slot_mut(process)
                        .memory
                        .remove(region)
                        .unwrap();
                    for thread_idx in 0..TASKS {
                        let thread = ThreadSlot(thread_idx);
                        if self.scheduler.task(thread).process == process {
                            if let Wait::Receive {
                                pointer, capacity, ..
                            } = self.scheduler.task(thread).wait
                            {
                                if region.contains(pointer, capacity) {
                                    self.scheduler.task_mut(thread).context.registers[8] =
                                        Errno::Efault.to_abi();
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
        &self.processes.slot(self.scheduler.current().process).memory
    }

    pub fn readable(&self, pointer: usize, length: usize) -> bool {
        self.memory().permits(pointer, length, READ)
    }

    pub fn writable(&self, pointer: usize, length: usize) -> bool {
        self.memory().permits(pointer, length, WRITE)
    }

    pub fn complete_receivers(&mut self, endpoint: usize, closed: bool) {
        for i in 0..TASKS {
            let thread = ThreadSlot(i);
            let task = *self.scheduler.task(thread);
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
                    Err(Errno::Epipe)
                } else {
                    self.ipc.receive(endpoint, capacity)
                };
                let value = match result {
                    Ok(None) => continue,
                    Ok(Some(message)) => {
                        self.calls
                            .bind_reply(&mut self.scheduler, message.reply_token, thread);
                        self.scheduler.task_mut(thread).context.registers[9] = message.reply_token;
                        match copy_to_user_within(
                            &self.processes.slot(task.process).memory,
                            pointer,
                            capacity,
                            &message.data[..message.length],
                        ) {
                            Ok(()) => message.length as u32,
                            Err(e) => e.to_abi(),
                        }
                    }
                    Err(error) => error.to_abi(),
                };
                // Blocked ecall already advanced mepc and was saved. Update its
                // own return register before making the task runnable.
                self.scheduler.task_mut(thread).context.registers[8] = value;
                self.scheduler.wake(thread);
                if let Wait::Receive { .. } = task.wait {
                    self.scheduler.prefer(thread);
                }
                if !self.logs.wake {
                    self.logs.wake = true;
                    let _ = writeln!(self.console, "[ipc] receiver woke task={}", task.id.raw());
                }
            }
        }
    }

    pub fn terminate(&mut self) {
        let process = self.scheduler.current().process;
        let exiting = self.scheduler.current;
        self.calls.deactivate(exiting);
        self.calls
            .fail_for_server(&mut self.scheduler, exiting, Errno::Epipe);
        self.scheduler.terminate();
        if self
            .scheduler
            .tasks
            .iter()
            .any(|t| t.process == process && t.state != State::Terminated)
        {
            return;
        }
        let owner = self.processes.slot(process).id;
        self.sentinel.release_wake_on_exit(owner);
        self.dma.release_process(owner);
        self.processes.slot_mut(process).state = Lifecycle::Dying;
        let objects: [Object; ipc::MAX_ENDPOINTS] = core::array::from_fn(|i| self.ipc.object(i));
        let ipc = &self.ipc;
        self.calls
            .fail_where(&mut self.scheduler, Errno::Epipe, |call, _| {
                ipc.owner(call.endpoint()) == Some(process)
            });
        let closed = self.ipc.remove_task(process);
        for (i, is_closed) in closed.iter().enumerate() {
            if *is_closed {
                self.invalidate_object(objects[i], None);
            }
        }
        for capability in self
            .processes
            .slot(process)
            .capabilities
            .objects()
            .into_iter()
            .flatten()
        {
            if let Object::Memory {
                owner: mem_owner, ..
            } = capability.object
            {
                if mem_owner == owner {
                    self.invalidate_object(capability.object, Some(process));
                }
            }
        }
        self.processes.slot_mut(process).capabilities.clear();
        self.allocator.release_owner(owner);
        self.processes.slot_mut(process).state = Lifecycle::Free;
        self.processes.slot_mut(process).memory = AddressSpace::EMPTY;
        self.pmp_images[process.index()] = None;
        for (ep, is_closed) in closed.into_iter().enumerate() {
            if is_closed {
                self.complete_receivers(ep, true);
            }
        }
    }

    pub fn spawn_elf(&mut self, bytes: &[u8], dma_driver: bool) -> Result<Pid, Errno> {
        let image = vela_kernel::elf::Image::parse(bytes).map_err(|_| Errno::Enoexec)?;
        let process = self.processes.create(AddressSpace::EMPTY)?;
        let owner = self.processes.slot(process).id;
        let result = (|| {
            let mut space = AddressSpace::EMPTY;
            let bias = if image.pic {
                let end = image
                    .segments()
                    .iter()
                    .map(|s| s.region.base + s.region.size)
                    .max()
                    .ok_or(Errno::Enoexec)?;
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
                        .ok_or(Errno::Enoexec)?;
                    if segment.region.base % size != 0 {
                        return Err(Errno::Enoexec);
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
                let data = image.data(i).ok_or(Errno::Enoexec)?;
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
                .map_err(|_| Errno::Enoexec)?;
            let entry = bias + image.entry;
            let stack = self.allocator.allocate(owner, 4096)?;
            space.insert(stack, READ | WRITE)?;
            let arg = if dma_driver {
                if !self.dma.acquire(owner) {
                    return Err(Errno::Ebusy);
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
            self.processes.slot_mut(process).memory = space;
            self.pmp_images[process.index()] = Some((space.revision, prepared));
            Ok(owner)
        })();
        if result.is_err() {
            self.dma.release_process(owner);
            self.allocator.release_owner(owner);
            *self.processes.slot_mut(process) = vela_kernel::process::Process::EMPTY;
        }
        result
    }

    pub fn complete_dma(&mut self, success: bool) {
        let Some(owner) = self.dma.finish(success) else {
            return;
        };
        let Some(process) = self.processes.find_alive(owner) else {
            return;
        };
        if success {
            self.processes
                .slot_mut(process)
                .memory
                .dma_access(soc::dma::page(), true)
                .unwrap();
        }
        for i in 0..TASKS {
            let thread = ThreadSlot(i);
            let t = self.scheduler.task_mut(thread);
            if t.process == process && t.state == State::Blocked && matches!(t.wait, Wait::Dma) {
                t.context.registers[8] = if success { 0 } else { Errno::Eio.to_abi() };
                self.scheduler.wake(thread);
            }
        }
    }
}
