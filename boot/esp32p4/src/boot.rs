use core::fmt::Write;
use vela_kernel::{
    capability::{self as cap, Capability},
    ipc::Ipc,
    memory::RegionAllocator,
    process::{AddressSpace, EXECUTE, Lifecycle, Processes, READ, WRITE},
    sched::{Scheduler, TASKS},
    task::Region,
};

use crate::{
    arch, board, pmp,
    runtime::{CONSOLE_RX, Call, RUNTIME, Runtime, create_endpoint, grant},
    soc, users,
};

unsafe extern "C" {
    static _user_code_start: u8;
    static _user_code_end: u8;
    static _user_stack0: u8;
    static _user_stack1: u8;
    static _user_stack2: u8;
    static _stack_top: u8;
    static _stack_bottom: u8;
    static _dma_page: u8;
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
        size: core::ptr::addr_of!(_user_code_end) as usize
            - core::ptr::addr_of!(_user_code_start) as usize,
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
    let rx_event = processes.slots[2]
        .capabilities
        .install(Capability {
            object: CONSOLE_RX,
            rights: cap::READ | cap::WAIT | cap::ACK,
        })
        .unwrap();
    let entries = [
        users::procfs_main as *const () as usize,
        users::shell_main as *const () as usize,
        users::console_main as *const () as usize,
    ];
    let args = [
        [request, proc_reply, 0],
        [reply, shell_request, input],
        [console_input, rx_event, 0],
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
        "[mm] PMP initialized\n[ipc] endpoints initialized\n[task] procfs started\n[task] shell started\n[task] console started (USB RX event)"
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
            core::ptr::addr_of!(_dma_page) as usize,
        ),
        ipc,
        calls: [Call::EMPTY; TASKS],
        idle_frame: idle,
        in_idle: false,
        dma: vela_kernel::dma::Lease::Free,
        dma_length: 0,
        dma_deadline: 0,
        sentinel_epoch: None,
        sentinel_ready: false,
        sentinel_wake: None,
        sentinel_sequence: 0,
        sentinel_pending: false,
        sentinel_command: 0,
        sentinel_argument: 0,
        sentinel_diagnostics: 0,
        sentinel_hz: 0,
        heartbeat_link: Default::default(),
        display_mode: 1,
        display_stage: u8::MAX,
        sentinel_heartbeat: true,
        sentinel_next_beat: 0,
        sentinel_notifications: 0,
        sentinel_last_publish: 0,
        usb_rx_interrupts: 0,
        logged_block: false,
        logged_wake: false,
        logged_idle: false,
    };
    unsafe {
        // SAFETY: once-only initialization. No reference survives assembly entry.
        let runtime = (*RUNTIME.0.get()).write(runtime);
        let _ = writeln!(runtime.console, "[sched] preemptive scheduler online");
        soc::timer::start(vela_trap_entry as *const () as usize);
        runtime.console.route_rx_irq();
        if let Some(boot) = soc::sentinel::start() {
            runtime.sentinel_epoch = Some(boot.epoch);
            runtime.sentinel_hz = boot.hz;
        }
        arch::enter(&runtime.scheduler.current().context);
    }
}
