#![no_std]
#![no_main]
use core::{cell::UnsafeCell, fmt::Write, mem::MaybeUninit};
use vela_kernel::{
    ipc::{self, Ipc, Message},
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

struct Runtime {
    scheduler: Scheduler<arch::TrapFrame>,
    processes: Processes,
    active_space: Option<(usize, u32)>,
    console: soc::console::Console,
    ipc: Ipc,
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
    // Explicit boot policy. Kernel IPC module has no service names/protocols.
    let mut ipc = Ipc::new();
    let request = ipc.create(0).unwrap();
    let reply = ipc.create(1).unwrap();
    let input = ipc.create(1).unwrap();
    let shell_request = ipc.grant_send(0, request, 1).unwrap();
    let proc_reply = ipc.grant_send(1, reply, 0).unwrap();
    let console_input = ipc.grant_send(1, input, 2).unwrap();
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
    let mut processes = Processes::new();
    for stack in stacks {
        let mut memory = AddressSpace::EMPTY;
        memory.insert(code, READ | EXECUTE).unwrap();
        memory.insert(stack, READ | WRITE).unwrap();
        processes.create(memory).unwrap();
    }
    let contexts = core::array::from_fn(|i| {
        let mut frame =
            arch::TrapFrame::user(entries[i], stacks[i].base + stacks[i].size, args[i][0]);
        frame.registers[9] = args[i][1];
        frame.registers[10] = args[i][2];
        frame
    });
    // SAFETY: isolated physical regions, no users executing.
    assert!(unsafe { pmp::activate(code, stacks[0]) }, "PMP readback");
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
    let runtime = Runtime {
        scheduler: Scheduler::new(contexts, entries, stacks),
        console,
        processes,
        active_space: Some((0, 2)),
        ipc,
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
impl Runtime {
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
                pointer,
                capacity,
            } = task.wait
            {
                if waiting != endpoint {
                    continue;
                }
                let result = if closed {
                    Err(ipc::EPIPE)
                } else {
                    self.ipc.receive(endpoint, capacity)
                };
                let value = match result {
                    Ok(None) => continue,
                    Ok(Some(message)) => {
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
                if !self.logged_wake {
                    self.logged_wake = true;
                    let _ = writeln!(self.console, "[ipc] receiver woke task={}", task.id);
                }
            }
        }
    }
    fn terminate(&mut self) {
        let process = self.scheduler.current().process;
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
        let closed = self.ipc.remove_task(process);
        for (ep, is_closed) in closed.into_iter().enumerate() {
            if is_closed {
                self.complete_receivers(ep, true);
            }
        }
    }
    fn dispatch(&mut self, frame: &mut arch::TrapFrame, switch: &mut bool) -> u32 {
        let a0 = frame.registers[8];
        let a1 = frame.registers[9];
        let a2 = frame.registers[10];
        let a3 = frame.registers[11];
        let current = self.scheduler.current().process;
        match frame.registers[15] {
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
                    || priority > 3
                    || affinity != 1
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
            syscall::ENDPOINT_CREATE => self.ipc.create(current).unwrap_or_else(|e| e),
            syscall::ENDPOINT_CLOSE => match self.ipc.close(current, a0) {
                Ok(endpoint) => {
                    if let Some(ep) = endpoint {
                        self.complete_receivers(ep, true);
                    }
                    0
                }
                Err(e) => e,
            },
            syscall::SEND => {
                let length = a2 as usize;
                if length > ipc::MESSAGE_BYTES {
                    return ipc::EMSGSIZE;
                }
                if !self.readable(a1 as usize, length) {
                    return syscall::EFAULT;
                }
                // Validate namespace/rights before touching sender's data.
                if let Err(e) = self.ipc.resolve(current, a0, ipc::SEND) {
                    return e;
                }
                let mut message = Message::EMPTY;
                message.length = length;
                for i in 0..length {
                    // SAFETY: checked sender mapping, copy into a kernel-owned message.
                    message.data[i] = unsafe { (a1 as *const u8).add(i).read() };
                }
                match self.ipc.send(current, a0, message) {
                    Ok(ep) => {
                        self.complete_receivers(ep, false);
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
                let ep = match self.ipc.resolve(current, a0, ipc::RECEIVE) {
                    Ok(ep) => ep,
                    Err(e) => return e,
                };
                match self.ipc.receive(ep, capacity) {
                    Ok(Some(message)) => {
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
        if !unsafe { pmp::activate(memory.maps[0].region, memory.maps[1].region) } {
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
