#![no_std]
#![no_main]
use core::{cell::UnsafeCell, fmt::Write, mem::MaybeUninit};
use vela_kernel::{
    capability::{self as cap, Capability, Object},
    ipc::{self, Ipc, Message},
    memory::RegionAllocator,
    process::{AddressSpace, EXECUTE, Lifecycle, Processes, READ, WRITE},
    sched::{Scheduler, TASKS},
    task::{Region, State, Wait},
};
#[path = "../../../arch/riscv32/trap.rs"]
mod arch;
#[path = "../../../bsp/m5stack-tab5/mod.rs"]
mod board;
#[path = "../../../pac/esp32p4-v13/mod.rs"]
mod pac;
#[path = "../../../arch/riscv32/pmp.rs"]
mod pmp;
#[path = "../../../soc/esp32p4-v13/mod.rs"]
mod soc;
#[path = "../../../abi/syscall.rs"]
mod syscall;
#[path = "../../../userspace/services.rs"]
mod users;
core::arch::global_asm!(include_str!("../entry.S"));

#[derive(Clone, Copy)]
struct Call {
    generation: u32,
    active: bool,
    endpoint: usize,
    server: Option<usize>,
}
impl Call {
    const EMPTY: Self = Self {
        generation: 0,
        active: false,
        endpoint: 0,
        server: None,
    };
}
struct Runtime {
    scheduler: Scheduler<arch::TrapFrame>,
    processes: Processes,
    active_space: Option<(usize, u32)>,
    pmp_images: [Option<(u32, pmp::Prepared)>; vela_kernel::process::PROCESSES],
    allocator: RegionAllocator,
    console: soc::console::Console,
    ipc: Ipc,
    calls: [Call; TASKS],
    idle_frame: arch::TrapFrame,
    in_idle: bool,
    logged_block: bool,
    logged_wake: bool,
    logged_idle: bool,
}
struct BootCell(UnsafeCell<MaybeUninit<Runtime>>);
// SAFETY: hart0 only, HP1 reset, MIE=0 in every Rust kernel path. The M-mode
// WFI loop has no Rust references and never accesses this cell. Initialization
// precedes timer enable; outermost traps are the only later borrowers.
unsafe impl Sync for BootCell {}
static RUNTIME: BootCell = BootCell(UnsafeCell::new(MaybeUninit::uninit()));
unsafe extern "C" {
    static _user_code_start: u8;
    static _user_stack0: u8;
    static _user_stack1: u8;
    static _user_stack2: u8;
    static _stack_top: u8;
    static _stack_bottom: u8;
    static _bss_end: u8;
    fn vela_trap_entry();
}
#[unsafe(no_mangle)]
extern "C" fn rust_main() -> ! {
    // SAFETY: M-mode startup owns scratch and console, IRQs masked.
    unsafe {
        core::arch::asm!("csrw mscratch, zero", options(nostack));
    }
    let mut console = unsafe { soc::console::Console::new() };
    assert_eq!(arch::hart_id(), 0);
    assert!(unsafe { soc::memory_ready() });
    let _ = writeln!(
        console,
        "VELA microkernel\nSpace Flow Project\n[board] {}",
        board::NAME
    );
    let code = Region {
        base: core::ptr::addr_of!(_user_code_start) as usize,
        size: 4096,
    };
    let stacks = [
        Region {
            base: core::ptr::addr_of!(_user_stack0) as usize,
            size: 4096,
        },
        Region {
            base: core::ptr::addr_of!(_user_stack1) as usize,
            size: 4096,
        },
        Region {
            base: core::ptr::addr_of!(_user_stack2) as usize,
            size: 4096,
        },
    ];
    let mut processes = Processes::new();
    for stack in stacks {
        let mut memory = AddressSpace::EMPTY;
        memory.insert(code, READ | EXECUTE).unwrap();
        memory.insert(stack, READ | WRITE).unwrap();
        processes.create(memory).unwrap();
    }
    // Explicit boot policy. Kernel IPC module has no service names/protocols.
    let mut ipc = Ipc::new();
    let request = create_endpoint(&mut ipc, &mut processes, 0).unwrap();
    let reply = create_endpoint(&mut ipc, &mut processes, 1).unwrap();
    let input = create_endpoint(&mut ipc, &mut processes, 1).unwrap();
    let shell_request = grant(&mut processes, 0, request, 1, cap::SEND).unwrap();
    let proc_reply = grant(&mut processes, 1, reply, 0, cap::SEND).unwrap();
    let console_input = grant(&mut processes, 1, input, 2, cap::SEND).unwrap();
    let entries = [
        users::procfs_main as *const () as usize,
        users::shell_main as *const () as usize,
        users::console_main as *const () as usize,
    ];
    let args = [
        [request, proc_reply, 0],
        [reply, shell_request, input],
        [console_input, 0, 0],
    ];
    let contexts = core::array::from_fn(|i| {
        let mut frame =
            arch::TrapFrame::user(entries[i], stacks[i].base + stacks[i].size, args[i][0]);
        frame.registers[9] = args[i][1];
        frame.registers[10] = args[i][2];
        frame
    });
    // SAFETY: isolated physical regions, no users executing.
    let initial_pmp = pmp::Prepared::new(&processes.slots[0].memory).unwrap();
    assert!(unsafe { initial_pmp.activate() }, "PMP readback");
    let _ = writeln!(
        console,
        "[mm] PMP initialized\n[ipc] endpoints initialized\n[task] procfs started\n[task] shell started\n[task] console started (bounded 100Hz polling)"
    );
    let mut idle = arch::TrapFrame::user(
        arch::vela_idle_entry as *const () as usize,
        core::ptr::addr_of!(_stack_top) as usize,
        0,
    );
    idle.mstatus = (3 << 11) | (1 << 7); // MPP=M, MPIE=1, FS=off
    let mut scheduler = Scheduler::new(contexts, entries, stacks);
    scheduler.tasks[0].priority = 0;
    scheduler.tasks[0].effective_priority = 0;
    scheduler.tasks[2].priority = 2;
    scheduler.tasks[2].effective_priority = 2;
    processes.slots[2].priority_ceiling = 2;
    let runtime = Runtime {
        scheduler,
        console,
        active_space: Some((0, 2)),
        pmp_images: core::array::from_fn(|i| {
            if processes.slots[i].state == Lifecycle::Alive {
                Some((
                    processes.slots[i].memory.revision,
                    pmp::Prepared::new(&processes.slots[i].memory).unwrap(),
                ))
            } else {
                None
            }
        }),
        processes,
        allocator: RegionAllocator::new(
            core::ptr::addr_of!(_bss_end) as usize,
            core::ptr::addr_of!(_stack_bottom) as usize,
        ),
        ipc,
        calls: [Call::EMPTY; TASKS],
        idle_frame: idle,
        in_idle: false,
        logged_block: false,
        logged_wake: false,
        logged_idle: false,
    };
    unsafe {
        // SAFETY: once-only initialization. No reference survives assembly entry.
        let runtime = (*RUNTIME.0.get()).write(runtime);
        let _ = writeln!(runtime.console, "[sched] preemptive scheduler online");
        soc::timer::start(vela_trap_entry as *const () as usize);
        arch::enter(&runtime.scheduler.current().context);
    }
}

/// Kernel copy only, caller validated a writable task-local stack range. No user
/// executes during this copy and no Rust reference aliases the user buffer.
unsafe fn deliver(pointer: usize, message: &Message) {
    for i in 0..message.length {
        unsafe {
            (pointer as *mut u8).add(i).write(message.data[i]);
        }
    }
}
fn create_endpoint(ipc: &mut Ipc, processes: &mut Processes, owner: usize) -> Result<u32, u32> {
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
fn grant(
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
    fn bind_reply(&mut self, token: u32, server: usize) {
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
    fn fail_call(&mut self, client: usize, error: u32) {
        self.calls[client].active = false;
        if matches!(self.scheduler.tasks[client].wait, Wait::Reply { .. }) {
            self.scheduler.tasks[client].context.registers[8] = error;
            self.scheduler.wake(client);
        }
    }
    fn expire_calls(&mut self, uptime: u64) {
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
    fn endpoint(&self, process: usize, handle: u32, rights: u16) -> Result<usize, u32> {
        let capability = self.processes.slots[process]
            .capabilities
            .get(handle, rights)?;
        self.ipc.resolve(capability.object)
    }
    fn invalidate_object(&mut self, object: Object, keep_owner: Option<usize>) {
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
    fn memory(&self) -> &AddressSpace {
        &self.processes.slots[self.scheduler.current().process].memory
    }
    fn readable(&self, pointer: usize, length: usize) -> bool {
        self.memory().permits(pointer, length, READ)
    }
    fn writable(&self, pointer: usize, length: usize) -> bool {
        self.memory().permits(pointer, length, WRITE)
    }
    fn complete_receivers(&mut self, endpoint: usize, closed: bool) {
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
    fn terminate(&mut self) {
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
    fn spawn_elf(&mut self, bytes: &[u8]) -> Result<u32, u32> {
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
            let prepared = pmp::Prepared::new(&space)?;
            unsafe {
                core::ptr::write_bytes(stack.base as *mut u8, 0, stack.size);
                core::arch::asm!("fence.i", options(nostack));
            }
            let frame = arch::TrapFrame::user(entry, stack.base + stack.size, 0);
            self.scheduler
                .create_thread(process, frame, stack, entry, 1, 1)?;
            self.processes.slots[process].memory = space;
            self.pmp_images[process] = Some((space.revision, prepared));
            Ok(owner)
        })();
        if result.is_err() {
            self.allocator.release_owner(owner);
            self.processes.slots[process] = vela_kernel::process::Process::EMPTY;
        }
        result
    }
    fn dispatch(&mut self, frame: &mut arch::TrapFrame, switch: &mut bool) -> u32 {
        let a0 = frame.registers[8];
        let a1 = frame.registers[9];
        let a2 = frame.registers[10];
        let a3 = frame.registers[11];
        let current = self.scheduler.current().process;
        match frame.registers[15] {
            syscall::SPAWN_HELLO => {
                if self.processes.slots[current].id != 2 {
                    return cap::EPERM;
                }
                let bytes: &[u8] = match a0 {
                    0 => include_bytes!(concat!(env!("OUT_DIR"), "/hello.elf")),
                    1 => include_bytes!(concat!(env!("OUT_DIR"), "/pic.elf")),
                    _ => return syscall::EINVAL,
                };
                self.spawn_elf(bytes)
                    .unwrap_or_else(|e| (-(e as i32)) as u32)
            }
            syscall::WRITE => {
                let length = a1 as usize;
                if length > 128 {
                    return syscall::EINVAL;
                }
                if !self.readable(a0 as usize, length) {
                    return syscall::EFAULT;
                }
                // SAFETY: validated entire readable range, single hart, IRQs masked.
                let bytes = unsafe { core::slice::from_raw_parts(a0 as *const u8, length) };
                self.console.write_bytes(bytes) as u32
            }
            syscall::YIELD => {
                *switch = true;
                0
            }
            syscall::EXIT => {
                self.terminate();
                *switch = true;
                0
            }
            syscall::THREAD_PRIORITY => {
                if a1 > u32::from(self.processes.slots[current].priority_ceiling) {
                    return syscall::EINVAL;
                }
                let Some(index) = self.scheduler.tasks.iter().position(|t| {
                    t.id == a0 && t.process == current && t.state != State::Terminated
                }) else {
                    return ipc::EPERM;
                };
                self.scheduler.tasks[index].priority = a1 as u8;
                self.scheduler.inherit();
                *switch = self.scheduler.should_preempt();
                0
            }
            syscall::IPC_CALL => {
                let length = a2 as usize;
                let capacity = frame.registers[12] as usize;
                let timeout = frame.registers[13];
                let thread = self.scheduler.current;
                if length > ipc::MESSAGE_BYTES
                    || capacity == 0
                    || capacity > ipc::MESSAGE_BYTES
                    || timeout > 6000
                {
                    return syscall::EINVAL;
                }
                if !self.readable(a1 as usize, length) || !self.writable(a3 as usize, capacity) {
                    return syscall::EFAULT;
                }
                let ep = match self.endpoint(current, a0, cap::SEND) {
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
                    message.data[i] = unsafe { (a1 as *const u8).add(i).read() };
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
                    pointer: a3 as usize,
                    capacity,
                    server,
                    until,
                });
                self.complete_receivers(ep, false);
                self.scheduler.inherit();
                *switch = true;
                0
            }
            syscall::IPC_REPLY => {
                let client = (a0 & 255).wrapping_sub(1) as usize;
                let Some(call) = self.calls.get(client).copied() else {
                    return ipc::EBADF;
                };
                if !call.active || call.generation != a0 >> 8 {
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
                if token != a0 {
                    return ipc::EPIPE;
                }
                if a2 as usize > capacity || a2 as usize > ipc::MESSAGE_BYTES {
                    return ipc::EMSGSIZE;
                }
                let target = self.scheduler.tasks[client].process;
                if !self.readable(a1 as usize, a2 as usize)
                    || !self.processes.slots[target]
                        .memory
                        .permits(pointer, capacity, WRITE)
                {
                    return syscall::EFAULT;
                }
                let mut message = Message::EMPTY;
                message.length = a2 as usize;
                for i in 0..message.length {
                    // SAFETY: validated server source and caller destination, sole hart.
                    message.data[i] = unsafe { (a1 as *const u8).add(i).read() };
                }
                unsafe {
                    deliver(pointer, &message);
                }
                self.scheduler.tasks[client].context.registers[8] = a2;
                self.calls[client].active = false;
                self.scheduler.wake(client);
                self.scheduler.inherit();
                self.scheduler.prefer(client);
                *switch = self.scheduler.should_preempt();
                0
            }
            syscall::CAP_GRANT | syscall::MEM_GRANT => {
                let peer = match self
                    .processes
                    .slots
                    .iter()
                    .position(|p| p.state == Lifecycle::Alive && p.id == a1)
                {
                    Some(p) if p != current => p,
                    _ => return syscall::EINVAL,
                };
                if a2 > u16::MAX as u32 {
                    return syscall::EINVAL;
                }
                let rights = a2 as u16;
                let source = match self.processes.slots[current]
                    .capabilities
                    .get(a0, cap::GRANT)
                {
                    Ok(c) => c,
                    Err(e) => return e,
                };
                if let Object::Memory { region, .. } = source.object {
                    if rights & !(cap::READ | cap::WRITE) != 0 || rights & cap::READ == 0 {
                        return syscall::EINVAL;
                    }
                    let handle = match grant(&mut self.processes, current, a0, peer, rights) {
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
                } else if frame.registers[15] == syscall::MEM_GRANT {
                    syscall::EINVAL
                } else {
                    grant(&mut self.processes, current, a0, peer, rights).unwrap_or_else(|e| e)
                }
            }
            syscall::CAP_REVOKE => {
                let c = match self.processes.slots[current]
                    .capabilities
                    .get(a0, cap::CONTROL)
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
            syscall::MEM_CAP => {
                let region = self
                    .memory()
                    .maps
                    .iter()
                    .find(|m| m.region.base == a0 as usize)
                    .map(|m| m.region);
                match region {
                    Some(region) => self.processes.slots[current]
                        .capabilities
                        .find(
                            Object::Memory {
                                owner: self.processes.slots[current].id,
                                region,
                            },
                            cap::CONTROL,
                        )
                        .unwrap_or(cap::EBADF),
                    None => cap::EBADF,
                }
            }
            syscall::MEM_ALLOC => {
                let owner = self.processes.slots[current].id;
                let region = match self.allocator.allocate(owner, a0 as usize) {
                    Ok(r) => r,
                    Err(e) => return (-(e as i32)) as u32,
                };
                if let Err(e) = self.processes.slots[current]
                    .memory
                    .insert(region, READ | WRITE)
                {
                    self.allocator.free(owner, region).unwrap();
                    return (-(e as i32)) as u32;
                }
                if let Err(e) = self.processes.slots[current]
                    .capabilities
                    .install(Capability {
                        object: Object::Memory { owner, region },
                        rights: cap::READ | cap::WRITE | cap::GRANT | cap::CONTROL,
                    })
                {
                    self.processes.slots[current].memory.remove(region).unwrap();
                    self.allocator.free(owner, region).unwrap();
                    return e;
                }
                // SAFETY: newly reserved physical SRAM, no aliases or user execution.
                unsafe {
                    core::ptr::write_bytes(region.base as *mut u8, 0, region.size);
                }
                frame.registers[9] = region.size as u32;
                region.base as u32
            }
            syscall::MEM_FREE => {
                let region = Region {
                    base: a0 as usize,
                    size: a1 as usize,
                };
                let owner = self.processes.slots[current].id;
                if !self.allocator.owns(owner, region)
                    || self.processes.slots[current]
                        .capabilities
                        .find(Object::Memory { owner, region }, cap::CONTROL)
                        .is_none()
                {
                    return ipc::EPERM;
                }
                if self.scheduler.tasks.iter().any(|t| {
                    t.process == current
                        && t.state != State::Terminated
                        && region.contains(t.stack.base, t.stack.size)
                }) {
                    return (-16i32) as u32;
                }
                self.invalidate_object(Object::Memory { owner, region }, None);
                self.allocator.free(owner, region).unwrap();
                // Cancel pending receives before the physical region can be reused.
                for i in 0..TASKS {
                    if self.scheduler.tasks[i].process != current {
                        continue;
                    }
                    if let Wait::Receive {
                        pointer, capacity, ..
                    } = self.scheduler.tasks[i].wait
                    {
                        if region.contains(pointer, capacity) {
                            self.scheduler.tasks[i].context.registers[8] = syscall::EFAULT;
                            self.scheduler.wake(i);
                        }
                    }
                }
                0
            }
            syscall::THREAD_CREATE => {
                let entry = a0 as usize;
                let stack = Region {
                    base: a1 as usize,
                    size: a2 as usize,
                };
                let priority = frame.registers[12];
                let affinity = frame.registers[13];
                if entry & 1 != 0
                    || !self.memory().permits(entry, 2, EXECUTE)
                    || stack.base & 15 != 0
                    || stack.size < 256
                    || stack.size & 15 != 0
                    || !self.writable(stack.base, stack.size)
                    || priority > u32::from(self.processes.slots[current].priority_ceiling)
                    || affinity != 1
                    || !self.allocator.owns(self.processes.slots[current].id, stack)
                {
                    return syscall::EINVAL;
                }
                let end = stack.base + stack.size;
                if self.scheduler.tasks.iter().any(|t| {
                    t.process == current
                        && t.state != State::Terminated
                        && stack.base < t.stack.base + t.stack.size
                        && t.stack.base < end
                }) {
                    return syscall::EINVAL;
                }
                let context = arch::TrapFrame::user(entry, end, a3);
                self.scheduler
                    .create_thread(
                        current,
                        context,
                        stack,
                        entry,
                        priority as u8,
                        affinity as u8,
                    )
                    .unwrap_or_else(|e| (-(e as i32)) as u32)
            }
            syscall::PROCESS_EXIT => {
                for thread in &mut self.scheduler.tasks {
                    if thread.process == current {
                        thread.state = State::Terminated;
                        thread.wait = Wait::None;
                    }
                }
                self.terminate();
                *switch = true;
                0
            }
            syscall::TICKS => self.scheduler.snapshot().uptime_ticks as u32,
            syscall::STATS => {
                if a1 as usize != syscall::STATS_BYTES {
                    return syscall::EINVAL;
                }
                if !self.writable(a0 as usize, syscall::STATS_BYTES) {
                    return syscall::EFAULT;
                }
                let s = self.scheduler.snapshot();
                let words = [
                    s.total_tasks,
                    s.runnable_tasks,
                    s.running_tasks,
                    s.last_task_id,
                    s.scheduler_ticks as u32,
                    (s.scheduler_ticks >> 32) as u32,
                    s.uptime_ticks as u32,
                    (s.uptime_ticks >> 32) as u32,
                    s.load_q16[0],
                    s.load_q16[1],
                    s.load_q16[2],
                ];
                for (i, word) in words.iter().enumerate() {
                    for (j, byte) in word.to_le_bytes().iter().enumerate() {
                        // SAFETY: checked writable private user stack, byte-aligned ABI.
                        unsafe {
                            (a0 as *mut u8).add(i * 4 + j).write(*byte);
                        }
                    }
                }
                0
            }
            syscall::ENDPOINT_CREATE => {
                create_endpoint(&mut self.ipc, &mut self.processes, current).unwrap_or_else(|e| e)
            }
            syscall::ENDPOINT_CLOSE => {
                let capability = match self.processes.slots[current].capabilities.get(a0, 0) {
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
                        .close(a0)
                        .unwrap();
                    self.complete_receivers(ep, false);
                }
                0
            }
            syscall::SEND => {
                let length = a2 as usize;
                if length > ipc::MESSAGE_BYTES {
                    return ipc::EMSGSIZE;
                }
                if !self.readable(a1 as usize, length) {
                    return syscall::EFAULT;
                }
                // Validate namespace/rights before touching sender's data.
                if let Err(e) = self.endpoint(current, a0, ipc::SEND) {
                    return e;
                }
                let mut message = Message::EMPTY;
                message.length = length;
                for i in 0..length {
                    // SAFETY: checked sender mapping, copy into a kernel-owned message.
                    message.data[i] = unsafe { (a1 as *const u8).add(i).read() };
                }
                match self
                    .ipc
                    .send(self.endpoint(current, a0, ipc::SEND).unwrap(), message)
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
            syscall::RECV => {
                let capacity = a2 as usize;
                if capacity == 0 || capacity > ipc::MESSAGE_BYTES || a3 & !syscall::NONBLOCK != 0 {
                    return syscall::EINVAL;
                }
                if !self.writable(a1 as usize, capacity) {
                    return syscall::EFAULT;
                }
                let ep = match self.endpoint(current, a0, ipc::RECEIVE) {
                    Ok(ep) => ep,
                    Err(e) => return e,
                };
                match self.ipc.receive(ep, capacity) {
                    Ok(Some(message)) => {
                        self.bind_reply(message.reply_token, self.scheduler.current);
                        frame.registers[9] = message.reply_token;
                        // SAFETY: checked writable receive buffer. No shared-memory pointers.
                        unsafe {
                            deliver(a1 as usize, &message);
                        }
                        message.length as u32
                    }
                    Err(e) => e,
                    Ok(None) if a3 & syscall::NONBLOCK != 0 => ipc::EAGAIN,
                    Ok(None) => {
                        // Check+wait registration+Blocked are one IRQ-masked
                        // transaction, so send cannot race and lose the wakeup.
                        self.scheduler.block(Wait::Receive {
                            endpoint: ep,
                            handle: a0,
                            pointer: a1 as usize,
                            capacity,
                        });
                        *switch = true;
                        if !self.logged_block {
                            self.logged_block = true;
                            let _ =
                                writeln!(self.console, "[ipc] recv blocked task={}", current + 1);
                        }
                        0 // replaced with completed length/error before wakeup
                    }
                }
            }
            syscall::CONSOLE_READ => {
                // Boot-assigned input authority: only console task owns RX.
                if current != 2 {
                    return ipc::EPERM;
                }
                let capacity = a1 as usize;
                if capacity == 0 || capacity > 32 {
                    return syscall::EINVAL;
                }
                if !self.writable(a0 as usize, capacity) {
                    return syscall::EFAULT;
                }
                let mut count = 0;
                while count < capacity {
                    let Some(byte) = self.console.read_byte() else {
                        break;
                    };
                    // SAFETY: checked writable console task buffer.
                    unsafe {
                        (a0 as *mut u8).add(count).write(byte);
                    }
                    count += 1;
                }
                count as u32
            }
            syscall::SLEEP => {
                if a0 == 0 || a0 > 6000 {
                    return syscall::EINVAL;
                }
                let until = self.scheduler.snapshot().uptime_ticks + u64::from(a0);
                self.scheduler.block(Wait::Sleep { until });
                *switch = true;
                0
            }
            _ => syscall::ENOSYS,
        }
    }
}
#[unsafe(no_mangle)]
extern "C" fn rust_trap(frame: &mut arch::TrapFrame) -> *const arch::TrapFrame {
    let interrupt = frame.mcause >> 31 != 0;
    let cause = frame.mcause & 0xfff;
    let from_idle = frame.mstatus & (3 << 11) != 0;
    // A kernel fault during boot may happen before RUNTIME is initialized.
    // Reject all M-mode entries except the known assembly idle IRQ first.
    if from_idle {
        let idle_pc = arch::vela_idle_entry as *const () as u32;
        if !interrupt || cause != 17 || frame.mepc.wrapping_sub(idle_pc) >= 8 {
            fatal(frame);
        }
    }
    // SAFETY: U-mode and the idle loop are entered only after initialization.
    // No Rust references survive mret. All Rust kernel paths have MIE=0.
    let runtime = unsafe { (*RUNTIME.0.get()).assume_init_mut() };
    if from_idle && !runtime.in_idle {
        fatal(frame);
    }
    let mut switch = false;
    if interrupt && cause == 17 {
        // SAFETY: sole timer owner under masked IRQs. Only bounded O(TASKS)
        // wakeup/accounting, no USB polling or console formatting in timer ISR.
        unsafe {
            soc::timer::acknowledge();
        }
        let uptime = unsafe { soc::timer::uptime_ticks() }.expect("SYSTIMER snapshot timeout");
        runtime.expire_calls(uptime);
        runtime.scheduler.tick(uptime);
        switch = true;
    } else if !interrupt && cause == 8 && !from_idle {
        frame.mepc = frame.mepc.wrapping_add(4);
        frame.registers[8] = runtime.dispatch(frame, &mut switch);
    } else if !interrupt && !from_idle {
        let _ = writeln!(
            runtime.console,
            "[fault] task={} cause={:#x} pc={:#x} value={:#x}",
            runtime.scheduler.current().id,
            frame.mcause,
            frame.mepc,
            frame.mtval
        );
        runtime.terminate();
        switch = true;
    } else {
        fatal(frame);
    }
    if !from_idle {
        runtime.scheduler.save(*frame);
    }
    let selected = if switch {
        runtime.scheduler.select(interrupt)
    } else {
        Some(runtime.scheduler.current)
    };
    // Preserve active trap bookkeeping, canonicalize target privilege and MPIL.
    let cause = frame.mcause & !((3 << 28) | (255 << 16));
    if selected.is_none() {
        runtime.in_idle = true;
        runtime.idle_frame.mcause = cause | (3 << 28) | (1 << 27);
        if !runtime.logged_idle {
            runtime.logged_idle = true;
            let _ = writeln!(runtime.console, "[idle] wfi (no runnable tasks)");
        }
        return &runtime.idle_frame;
    }
    runtime.in_idle = false;
    let task = &mut runtime.scheduler.tasks[runtime.scheduler.current];
    task.context.mcause = cause;
    let process = task.process;
    let memory = runtime.processes.slots[process].memory;
    // SAFETY: activate only selected task's RX code / RW stack before mret.
    if runtime.active_space != Some((process, memory.revision)) {
        let image = match runtime.pmp_images[process] {
            Some((revision, image)) if revision == memory.revision => image,
            _ => {
                let image = pmp::Prepared::new(&memory).unwrap();
                runtime.pmp_images[process] = Some((memory.revision, image));
                image
            }
        };
        if !unsafe { image.activate() } {
            fatal(frame);
        }
        runtime.active_space = Some((process, memory.revision));
    }
    &task.context
}
fn fatal(frame: &arch::TrapFrame) -> ! {
    // SAFETY: fatal takeover never resumes an interrupted console owner.
    unsafe {
        core::arch::asm!("csrci mstatus,8", options(nostack));
    }
    let mut console = unsafe { soc::console::Console::new() };
    let _ = writeln!(
        console,
        "[kernel trap] cause={:#x} pc={:#x} value={:#x}",
        frame.mcause, frame.mepc, frame.mtval
    );
    arch::halt()
}
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    // SAFETY: fatal takeover; no concurrent console and no return.
    unsafe {
        core::arch::asm!("csrci mstatus,8", options(nostack));
    }
    let mut console = unsafe { soc::console::Console::new() };
    let _ = writeln!(console, "[panic] {info}");
    arch::halt()
}
