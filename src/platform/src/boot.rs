use core::fmt::Write;
use vela_kernel::{
    call::CallTable,
    capability::{self as cap, Authority, Capability, Object},
    dma::{DmaController, DmaTable},
    event::EventTable,
    id::ThreadSlot,
    ipc::Ipc,
    lifecycle::{bind_event, create_endpoint, grant},
    memory::{MemoryClass, RegionAllocator},
    process::{PROCESSES, Processes},
    sched::Scheduler,
    stats::{CycleAccounting, FreqMode},
};

use crate::{
    arch, board,
    display::DisplayControl,
    dma::{DMA_CH_DW_GDMA_SCANOUT, DMA_CH_GDMA_AHB_CH1},
    runtime::{BootLogs, HartState, IRQ_SLOT_CONSOLE_RX, IRQ_SLOT_KBD, RUNTIME, Runtime},
    sentinel::SentinelClient,
    soc,
};

pub(crate) const EXT4FS_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ext4fs.elf"));
pub(crate) const SYSD_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sysd.elf"));
const INIT_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/init.elf"));
const CONSOLE_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/console.elf"));
pub(crate) const KBD_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/kbd.elf"));
pub(crate) const FBCON_ELF: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fbcon.elf"));

unsafe extern "C" {
    static _stack_top: u8;
    static _hp1_stack_top: u8;
    static _dma_page: u8;
    static _bss_end: u8;
    fn vela_trap_entry();
}

#[unsafe(no_mangle)]
extern "C" fn rust_main() -> ! {
    // SAFETY: M-mode startup owns scratch, console, and board I2C pins; IRQs masked.
    unsafe {
        core::arch::asm!("csrw mscratch, zero", options(nostack));
        arch::init_counters();
        board::init_i2c();
    }
    let mut console = unsafe { soc::console::Console::new() };
    assert_eq!(arch::hart_id(), 0);
    assert!(unsafe { soc::memory_ready() });
    let _ = writeln!(
        console,
        "VELA microkernel\nSpace Flow Project\n[board] {}",
        board::NAME
    );

    let mut idle0 = arch::TrapFrame::user(
        arch::vela_idle_entry as *const () as usize,
        core::ptr::addr_of!(_stack_top) as usize,
        0,
    );
    idle0.mstatus = (3 << 11) | (1 << 7); // MPP=M, MPIE=1, FS=off

    let mut idle1 = arch::TrapFrame::user(
        arch::vela_idle_entry as *const () as usize,
        core::ptr::addr_of!(_hp1_stack_top) as usize,
        0,
    );
    idle1.mstatus = (3 << 11) | (1 << 7); // MPP=M, MPIE=1, FS=off

    let mut events = EventTable::new();
    events
        .register(IRQ_SLOT_CONSOLE_RX, soc::console::RX_LINE)
        .expect("register console RX IRQ slot");
    events
        .register(IRQ_SLOT_KBD, soc::gpio_irq::LINE)
        .expect("register keyboard INT IRQ slot");

    let mut dma = DmaTable::new();
    dma.register(DMA_CH_GDMA_AHB_CH1, DmaController::GdmaAhb)
        .expect("register GDMA-AHB CH1 slot");
    dma.register(DMA_CH_DW_GDMA_SCANOUT, DmaController::DwGdmaAxi)
        .expect("register DW-GDMA CH0 scanout slot");

    // SAFETY: once-only initialization on hart0 with IRQs masked.
    let runtime = unsafe {
        (*RUNTIME.0.get()).write(Runtime {
            scheduler: Scheduler::empty(idle0),
            ext_contexts: [arch::ExtendedContext::ZERO; vela_kernel::sched::TASKS],
            processes: Processes::new(),
            harts: [
                HartState {
                    active_space: None,
                    idle_frame: idle0,
                    in_idle: false,
                    cycles: CycleAccounting::new(),
                    ipi_interrupts: 0,
                    pmp_shootdowns: 0,
                },
                HartState {
                    active_space: None,
                    idle_frame: idle1,
                    in_idle: true,
                    cycles: CycleAccounting::new(),
                    ipi_interrupts: 0,
                    pmp_shootdowns: 0,
                },
            ],
            pmp_images: [None; PROCESSES],
            allocator: RegionAllocator::new(
                core::ptr::addr_of!(_bss_end) as usize,
                core::ptr::addr_of!(_dma_page) as usize,
            ),
            console,
            ipc: Ipc::new(),
            calls: CallTable::new(),
            events,
            dma,
            sentinel: SentinelClient::new(),
            display: DisplayControl::new(),
            usb_storage: soc::usb_storage::UsbStorage::new(),
            sd_export_buffer: None,
            sd_export_owner: None,
            sd_export_interrupted: false,
            sd_epoch: 1,
            sd_partition: None,
            dma_interrupts: 0,
            sentinel_interrupts: 0,
            freq_mode: FreqMode::Auto,
            active_high_180mhz: false,
            tickless_armed: false,
            tickless_enter_16m: 0,
            tickless_period_16m: soc::timer::PERIOD_10MS_16M,
            sysd_pid: None,
            sd_endpoint: None,
            ext4_pid: None,
            fbcon_pid: None,
            kbd_pid: None,
            logs: BootLogs::EMPTY,
            last_fault: crate::trap::FaultReport::EMPTY,
        })
    };

    // Keep room in SRAM for service stacks and DMA by placing init in PSRAM.
    let (sysd, _, _) = runtime.load_elf(SYSD_ELF, false).expect("sysd ELF");
    let (init_proc, _, _) = runtime
        .load_elf_in(INIT_ELF, MemoryClass::Psram, false)
        .expect("init ELF");
    let (console_proc, _, _) = runtime.load_elf(CONSOLE_ELF, false).expect("console ELF");
    let (ext4fs, _, _) = runtime
        .load_elf_in(EXT4FS_ELF, MemoryClass::Psram, false)
        .expect("ext4fs ELF");
    runtime.ext4_pid = Some(ext4fs.pid);
    // Explicit boot policy: IPC endpoints, managed hardware RX IRQ binding, and service authorities.
    let vfs_request = create_endpoint(&mut runtime.ipc, &mut runtime.processes, sysd.slot).unwrap();
    let vfs_object = runtime
        .processes
        .slot(sysd.slot)
        .capabilities
        .get(vfs_request, cap::CONTROL)
        .unwrap()
        .object;
    runtime.ipc.retain_on_exit(vfs_object).unwrap();
    runtime.sysd_pid = Some(sysd.pid);
    let init_vfs = runtime
        .processes
        .slot_mut(init_proc.slot)
        .capabilities
        .install(Capability {
            object: vfs_object,
            rights: cap::SEND | cap::ADMIN | cap::PASS_ON_SPAWN,
        })
        .unwrap();

    // `init` owns the terminal input endpoint and delegates RECEIVE to spawned children (e.g. shell),
    // keeping `console` and `kbd` connected even if the child restarts.
    let input_owner =
        create_endpoint(&mut runtime.ipc, &mut runtime.processes, init_proc.slot).unwrap();
    let input_object = runtime
        .processes
        .slot(init_proc.slot)
        .capabilities
        .get(input_owner, cap::CONTROL)
        .unwrap()
        .object;
    let init_input = runtime
        .processes
        .slot_mut(init_proc.slot)
        .capabilities
        .install(Capability {
            object: input_object,
            rights: cap::RECEIVE | cap::PASS_ON_SPAWN,
        })
        .unwrap();

    let fs_request =
        create_endpoint(&mut runtime.ipc, &mut runtime.processes, ext4fs.slot).unwrap();
    runtime.sd_endpoint = Some(
        runtime
            .processes
            .slot(ext4fs.slot)
            .capabilities
            .get(fs_request, cap::CONTROL)
            .unwrap()
            .object,
    );
    let sysd_ext4 = grant(
        &mut runtime.processes,
        ext4fs.slot,
        fs_request,
        sysd.slot,
        cap::SEND,
    )
    .unwrap();
    let storage = runtime
        .processes
        .slot_mut(ext4fs.slot)
        .capabilities
        .install(Capability {
            object: Object::Authority(Authority::SdSystemPartition),
            rights: cap::READ | cap::WRITE,
        })
        .unwrap();

    let console_input = grant(
        &mut runtime.processes,
        init_proc.slot,
        input_owner,
        console_proc.slot,
        cap::SEND,
    )
    .unwrap();
    let rx_event = bind_event(
        &mut runtime.events,
        &mut runtime.processes,
        &mut runtime.scheduler,
        console_proc.slot,
        IRQ_SLOT_CONSOLE_RX,
        cap::READ | cap::WAIT | cap::ACK | cap::GRANT | cap::CONTROL,
    )
    .unwrap();
    let init_caps = &mut runtime.processes.slot_mut(init_proc.slot).capabilities;
    let supervisor = init_caps
        .install(Capability {
            object: Object::Authority(Authority::ServiceSupervisor(init_vfs)),
            rights: cap::CONTROL,
        })
        .unwrap();
    let kbd_supervisor = init_caps
        .install(Capability {
            object: Object::Authority(Authority::KeyboardSupervisor(input_owner)),
            rights: cap::CONTROL,
        })
        .unwrap();
    let spawn_cap = init_caps
        .install(Capability {
            object: Object::Authority(Authority::ProcessSpawn),
            rights: cap::EXECUTE | cap::CONTROL | cap::PASS_ON_SPAWN,
        })
        .unwrap();
    let display_cap = init_caps
        .install(Capability {
            object: Object::Authority(Authority::Display),
            rights: cap::CONTROL | cap::PASS_ON_SPAWN,
        })
        .unwrap();

    runtime
        .start_sysd(sysd, vfs_request, sysd_ext4.raw())
        .unwrap();
    runtime
        .start_loaded(
            init_proc,
            [
                init_vfs.raw(),
                init_input.raw(),
                spawn_cap.raw(),
                display_cap.raw(),
                supervisor.raw(),
                kbd_supervisor.raw(),
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
    runtime
        .start_loaded(ext4fs, [fs_request.raw(), storage.raw(), 0, 0, 0, 0], 1, 1)
        .unwrap();

    let (revision, initial_pmp, _) = runtime.pmp_images[sysd.slot.index()].unwrap();
    assert!(unsafe { initial_pmp.activate_initial() }, "PMP readback");
    runtime.pmp_images[sysd.slot.index()] = Some((revision, initial_pmp, 1));
    runtime.harts[0].active_space = Some((sysd.slot, revision));
    runtime.scheduler.activate_initial_on(0, ThreadSlot(0));

    let _ = writeln!(
        runtime.console,
        "[mm] PMP initialized\n[ipc] endpoints initialized\n[task] sysd started (/proc /sys /dev /sd)\n[task] init started\n[task] console started (USB RX event)\n[sched] preemptive scheduler online"
    );

    unsafe {
        // SAFETY: no Rust reference survives assembly entry into U-mode.
        soc::timer::start(vela_trap_entry as *const () as usize);
        soc::smp::init_hp0_ipi();
        runtime.console.route_rx_irq();
        soc::gpio_irq::route();
        if let Some(boot) = soc::sentinel::start() {
            runtime.sentinel.set_boot_info(boot.epoch, boot.hz);
        }
        let rates = soc::clock::rates_mhz();
        let now_16m = soc::timer::counter_16mhz().expect("SYSTIMER boot snapshot");
        runtime.harts[0]
            .cycles
            .start(arch::mcycle(), now_16m, rates.cpu);
        runtime.harts[1].cycles.start(0, now_16m, rates.cpu);
        let hp1_ok = soc::smp::start_hp1(arch::vela_hp1_entry);
        let _ = writeln!(
            runtime.console,
            "[smp] HP1 online={} L1-ICache1={}",
            u8::from(hp1_ok),
            u8::from(soc::memory_ready())
        );
        arch::restore_extended_context(&runtime.ext_contexts[0]);
        let initial_ctx = &raw const runtime.scheduler.tasks[0].context;
        soc::smp::mark_hart_in_user(0);
        // Kick HP1 so it picks up any runnable thread (e.g. console/init) immediately.
        soc::smp::send_ipi(1);
        arch::enter(initial_ctx);
    }
}

#[unsafe(no_mangle)]
extern "C" fn rust_hp1_main() -> ! {
    assert_eq!(arch::hart_id(), 1);
    unsafe {
        arch::init_counters();
        arch::disable_pie();
        soc::smp::init_hp1_clic_and_ipi(vela_trap_entry as *const () as usize);
    }
    soc::smp::KERNEL_LOCK.lock();
    let idle_ptr: *const arch::TrapFrame = unsafe {
        let runtime = (*RUNTIME.0.get()).assume_init_mut();
        let (_, initial_pmp, mask) = runtime.pmp_images[0].unwrap();
        assert!(initial_pmp.activate_initial(), "HP1 PMP readback");
        let rev = runtime
            .processes
            .slot(vela_kernel::id::ProcSlot(0))
            .memory
            .revision;
        runtime.pmp_images[0] = Some((rev, initial_pmp, mask | 2));
        runtime.harts[1].active_space = Some((vela_kernel::id::ProcSlot(0), rev));
        runtime.harts[1].in_idle = true;
        let rates = soc::clock::rates_mhz();
        let now_16m = soc::timer::counter_16mhz().unwrap_or(0);
        runtime.harts[1]
            .cycles
            .start(arch::mcycle(), now_16m, rates.cpu);
        &raw const runtime.harts[1].idle_frame
    };
    soc::smp::mark_hart_in_kernel(1);
    soc::smp::set_hp1_online();
    soc::smp::KERNEL_LOCK.unlock();
    unsafe { arch::enter(idle_ptr) }
}
