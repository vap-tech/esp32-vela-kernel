use core::fmt::Write;
use vela_kernel::{
    call::CallTable,
    capability::{self as cap, Authority, Capability, Object},
    id::ThreadSlot,
    ipc::Ipc,
    lifecycle::{create_endpoint, grant},
    memory::RegionAllocator,
    process::{PROCESSES, Processes},
    sched::Scheduler,
};

use crate::{
    arch, board,
    display::DisplayControl,
    dma::DmaChannel,
    runtime::{BootLogs, CONSOLE_RX, RUNTIME, Runtime},
    sentinel::SentinelClient,
    soc,
};

const PROCFS_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/procfs.elf"));
const SHELL_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shell.elf"));
const CONSOLE_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/console.elf"));

unsafe extern "C" {
    static _stack_top: u8;
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

    let mut idle = arch::TrapFrame::user(
        arch::vela_idle_entry as *const () as usize,
        core::ptr::addr_of!(_stack_top) as usize,
        0,
    );
    idle.mstatus = (3 << 11) | (1 << 7); // MPP=M, MPIE=1, FS=off

    // SAFETY: once-only initialization on hart0 with IRQs masked.
    let runtime = unsafe {
        (*RUNTIME.0.get()).write(Runtime {
            scheduler: Scheduler::empty(idle),
            processes: Processes::new(),
            active_space: None,
            pmp_images: [None; PROCESSES],
            allocator: RegionAllocator::new(
                core::ptr::addr_of!(_bss_end) as usize,
                core::ptr::addr_of!(_dma_page) as usize,
            ),
            console,
            ipc: Ipc::new(),
            calls: CallTable::new(),
            idle_frame: idle,
            in_idle: false,
            dma: DmaChannel::new(),
            sentinel: SentinelClient::new(),
            display: DisplayControl::new(),
            usb_rx_interrupts: 0,
            logs: BootLogs::EMPTY,
        })
    };

    // Load isolated standalone user ELF services into private SRAM allocations.
    let (procfs, _) = runtime.load_elf(PROCFS_ELF, false).expect("procfs ELF");
    let (shell, _) = runtime.load_elf(SHELL_ELF, false).expect("shell ELF");
    let (console_proc, _) = runtime.load_elf(CONSOLE_ELF, false).expect("console ELF");

    // Explicit boot policy: IPC endpoints, hardware RX event, and service authorities.
    let request = create_endpoint(&mut runtime.ipc, &mut runtime.processes, procfs.slot).unwrap();
    let reply = create_endpoint(&mut runtime.ipc, &mut runtime.processes, shell.slot).unwrap();
    let input = create_endpoint(&mut runtime.ipc, &mut runtime.processes, shell.slot).unwrap();
    let shell_request = grant(
        &mut runtime.processes,
        procfs.slot,
        request,
        shell.slot,
        cap::SEND,
    )
    .unwrap();
    let proc_reply = grant(
        &mut runtime.processes,
        shell.slot,
        reply,
        procfs.slot,
        cap::SEND,
    )
    .unwrap();
    let console_input = grant(
        &mut runtime.processes,
        shell.slot,
        input,
        console_proc.slot,
        cap::SEND,
    )
    .unwrap();
    let rx_event = runtime
        .processes
        .slot_mut(console_proc.slot)
        .capabilities
        .install(Capability {
            object: CONSOLE_RX,
            rights: cap::READ | cap::WAIT | cap::ACK,
        })
        .unwrap();
    let shell_caps = &mut runtime.processes.slot_mut(shell.slot).capabilities;
    let spawn_cap = shell_caps
        .install(Capability {
            object: Object::Authority(Authority::ProcessSpawn),
            rights: cap::EXECUTE | cap::GRANT | cap::CONTROL,
        })
        .unwrap();
    let display_cap = shell_caps
        .install(Capability {
            object: Object::Authority(Authority::Display),
            rights: cap::CONTROL | cap::GRANT,
        })
        .unwrap();
    let sentinel_cap = shell_caps
        .install(Capability {
            object: Object::Authority(Authority::Sentinel),
            rights: cap::READ | cap::WRITE | cap::WAIT | cap::CONTROL | cap::GRANT,
        })
        .unwrap();

    runtime
        .start_loaded(procfs, [request.raw(), proc_reply.raw(), 0, 0, 0, 0], 0, 1)
        .unwrap();
    runtime
        .start_loaded(
            shell,
            [
                reply.raw(),
                shell_request.raw(),
                input.raw(),
                spawn_cap.raw(),
                display_cap.raw(),
                sentinel_cap.raw(),
            ],
            1,
            1,
        )
        .unwrap();
    runtime
        .start_loaded(
            console_proc,
            [console_input.raw(), rx_event.raw(), 0, 0, 0, 0],
            2,
            2,
        )
        .unwrap();

    // SAFETY: isolated physical regions, no users executing yet.
    let (revision, initial_pmp) = runtime.pmp_images[procfs.slot.index()].unwrap();
    assert!(unsafe { initial_pmp.activate() }, "PMP readback");
    runtime.active_space = Some((procfs.slot, revision));
    runtime.scheduler.activate_initial(ThreadSlot(0));

    let _ = writeln!(
        runtime.console,
        "[mm] PMP initialized\n[ipc] endpoints initialized\n[task] procfs started\n[task] shell started\n[task] console started (USB RX event)\n[sched] preemptive scheduler online"
    );

    unsafe {
        // SAFETY: no Rust reference survives assembly entry into U-mode.
        soc::timer::start(vela_trap_entry as *const () as usize);
        runtime.console.route_rx_irq();
        if let Some(boot) = soc::sentinel::start() {
            runtime.sentinel.set_boot_info(boot.epoch, boot.hz);
        }
        let initial_ctx = &raw const runtime.scheduler.tasks[0].context;
        arch::enter(initial_ctx);
    }
}
