#![no_std]
#![no_main]
use core::{cell::UnsafeCell, fmt::Write, mem::MaybeUninit};
use vela_kernel::{sched::Scheduler, task::Region};
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
#[path = "../../../userspace/first_tasks.rs"]
mod users;
core::arch::global_asm!(include_str!("../entry.S"));

struct Runtime {
    scheduler: Scheduler<arch::TrapFrame>,
    console: soc::console::Console,
}
struct BootCell(UnsafeCell<MaybeUninit<Runtime>>);
// SAFETY: hart0 only, HP1 reset; MIE=0 throughout kernel execution. No nested
// handlers. Initialization happens once before entering U-mode; users cannot
// map this cell. Only rust_trap borrows it thereafter, for one trap at a time.
unsafe impl Sync for BootCell {}
static RUNTIME: BootCell = BootCell(UnsafeCell::new(MaybeUninit::uninit()));
unsafe extern "C" {
    static _user_code_start: u8;
    static _user_stack0: u8;
    static _user_stack1: u8;
    fn vela_trap_entry();
}
#[unsafe(no_mangle)]
extern "C" fn rust_main() -> ! {
    // SAFETY: initialize kernel trap-stack sentinel before CSR/PMP setup.
    unsafe {
        core::arch::asm!("csrw mscratch, zero", options(nostack));
    }
    // SAFETY: sole console owner on boot hart, IRQs disabled.
    let mut console = unsafe { soc::console::Console::new() };
    assert_eq!(arch::hart_id(), 0);
    assert!(unsafe { soc::memory_ready() });
    let _ = writeln!(console, "VELA microkernel\n[board] {}", board::NAME);
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
    ];
    let entry = users::vela_user_entry as *const () as usize;
    let contexts = core::array::from_fn(|i| {
        arch::TrapFrame::user(entry, stacks[i].base + stacks[i].size, i as u32)
    });
    // SAFETY: mapped isolated regions, no users yet, no inherited locked entries.
    assert!(unsafe { pmp::activate(code, stacks[0]) }, "PMP readback");
    let _ = writeln!(
        console,
        "[mm] PMP initialized\n[task] init spawned\n[task] worker spawned"
    );
    let runtime = Runtime {
        scheduler: Scheduler::new(contexts, entry, code, stacks),
        console,
    };
    unsafe {
        // SAFETY: unique once-only boot write; no reference survives user entry.
        let runtime = (*RUNTIME.0.get()).write(runtime);
        let _ = writeln!(
            runtime.console,
            "[sched] preemptive scheduler started (100 Hz)"
        );
        soc::timer::start(vela_trap_entry as *const () as usize);
        arch::enter(&runtime.scheduler.current().context);
    }
}
fn stats(runtime: &mut Runtime) {
    let s = runtime.scheduler.snapshot();
    let _ = writeln!(
        runtime.console,
        "[sched] ticks={} up={} runnable={}/{} running={} last={} preempt={}/{} load_q16={}/{}/{}",
        s.scheduler_ticks,
        s.uptime_ticks,
        s.runnable_tasks,
        s.total_tasks,
        s.running_tasks,
        s.last_task_id,
        runtime.scheduler.tasks[0].preemptions,
        runtime.scheduler.tasks[1].preemptions,
        s.load_q16[0],
        s.load_q16[1],
        s.load_q16[2]
    );
}
#[unsafe(no_mangle)]
extern "C" fn rust_trap(frame: &mut arch::TrapFrame) -> *const arch::TrapFrame {
    if frame.mstatus & (3 << 11) != 0 {
        fatal(frame);
    }
    // SAFETY: outermost trap from U, unique access under masked machine IRQs.
    let runtime = unsafe { (*RUNTIME.0.get()).assume_init_mut() };
    let interrupt = frame.mcause >> 31 != 0;
    let cause = frame.mcause & 0xfff; // P4 CLIC exception code, not all lower31 bits
    let mut switch = false;
    if interrupt && cause == 17 {
        unsafe {
            soc::timer::acknowledge();
        }
        // SAFETY: this handler exclusively owns SYSTIMER snapshots.
        let uptime = unsafe { soc::timer::uptime_ticks() }.expect("SYSTIMER snapshot timeout");
        runtime.scheduler.tick(uptime);
        switch = true;
        // Rare diagnostic also demonstrates actual timer-preempted user contexts.
        if runtime.scheduler.snapshot().scheduler_ticks % 100 == 0 {
            stats(runtime);
        }
        // Optional isolation probes: 'k' kernel SRAM, 'm' kernel MMIO, 's' peer stack.
        if let Some(byte) = runtime.console.read_byte() {
            if byte == b'a' {
                frame.mepc = users::vela_user_abi_probe as *const () as u32;
            }
            let address = match byte {
                b'k' => 0x4ff40000,
                b'm' => pac::USB_FIFO,
                b's' => {
                    runtime.scheduler.tasks[1 - runtime.scheduler.current]
                        .stack
                        .base
                }
                _ => 0,
            };
            if address != 0 {
                let _ = writeln!(
                    runtime.console,
                    "[probe] task={} read {:#x}",
                    runtime.scheduler.current().id,
                    address
                );
                frame.mepc = users::vela_user_probe as *const () as u32;
                frame.registers[8] = address as u32;
            }
        }
    } else if !interrupt && cause == 8 {
        frame.mepc = frame.mepc.wrapping_add(4); // ecall is always 32 bits
        let a0 = frame.registers[8];
        let a1 = frame.registers[9];
        let number = frame.registers[15]; // a7
        let result = match number {
            syscall::WRITE => {
                let length = a1 as usize;
                if length > 128 {
                    syscall::EINVAL
                } else if !runtime.scheduler.current().readable(a0 as usize, length) {
                    syscall::EFAULT
                } else {
                    // SAFETY: checked against current task's physical PMP mappings,
                    // no other hart or IRQ can change them while copying to console.
                    let bytes = unsafe { core::slice::from_raw_parts(a0 as *const u8, length) };
                    runtime.console.write_bytes(bytes) as u32
                }
            }
            syscall::YIELD => {
                switch = true;
                0
            }
            syscall::EXIT => {
                runtime.scheduler.terminate();
                switch = true;
                0
            }
            syscall::TICKS => runtime.scheduler.snapshot().uptime_ticks as u32,
            syscall::STATS => {
                if a1 as usize != syscall::STATS_BYTES {
                    syscall::EINVAL
                } else if !runtime
                    .scheduler
                    .current()
                    .stack
                    .contains(a0 as usize, syscall::STATS_BYTES)
                {
                    syscall::EFAULT
                } else {
                    let s = runtime.scheduler.snapshot();
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
                    // SAFETY: checked writable private stack, byte copy supports unaligned buffers.
                    for (i, word) in words.iter().enumerate() {
                        for (j, byte) in word.to_le_bytes().iter().enumerate() {
                            unsafe {
                                (a0 as *mut u8).add(i * 4 + j).write(*byte);
                            }
                        }
                    }
                    0
                }
            }
            _ => syscall::ENOSYS,
        };
        frame.registers[8] = result;
    } else if !interrupt {
        let _ = writeln!(
            runtime.console,
            "[fault] task={} mcause={:#x} mepc={:#x} mtval={:#x}",
            runtime.scheduler.current().id,
            frame.mcause,
            frame.mepc,
            frame.mtval
        );
        runtime.scheduler.terminate();
        switch = true;
    } else {
        fatal(frame);
    }
    runtime.scheduler.save(*frame);
    if switch && runtime.scheduler.select(interrupt).is_none() {
        let _ = writeln!(runtime.console, "[sched] no runnable tasks");
        arch::halt(); // tickless halt; no idle task exists yet
    }
    // CLIC mcause belongs to the *active trap*, not the suspended task. Its
    // interrupt/exception return bookkeeping must not be replaced with a cause
    // from an earlier timer interrupt when switching after a synchronous fault.
    runtime.scheduler.tasks[runtime.scheduler.current]
        .context
        .mcause = frame.mcause;
    let task = runtime.scheduler.current();
    // SAFETY: no user is running, change only its code and private stack mappings.
    if !unsafe { pmp::activate(task.code, task.stack) } {
        fatal(frame);
    }
    if !interrupt && cause != 8 {
        let _ = writeln!(
            runtime.console,
            "[resume] task={} pc={:#x} cause={:#x} status={:#x} sp={:#x}",
            task.id, task.context.mepc, task.context.mcause, task.context.mstatus, task.context.sp
        );
    }
    &task.context
}
fn fatal(frame: &arch::TrapFrame) -> ! {
    unsafe {
        core::arch::asm!("csrci mstatus,8", options(nostack));
    }
    // SAFETY: fatal path abandons any other console owner permanently.
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
    unsafe {
        core::arch::asm!("csrci mstatus,8", options(nostack));
    }
    // SAFETY: fatal takeover; IRQs masked, never resumes previous console owner.
    let mut console = unsafe { soc::console::Console::new() };
    let _ = writeln!(console, "[panic] {info}");
    arch::halt()
}
