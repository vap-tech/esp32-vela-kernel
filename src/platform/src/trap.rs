use core::fmt::Write;
use vela_kernel::{id::Ticks, sched::HARTS, task::Context};

use crate::{arch, dma::DMA_CH_GDMA_AHB_CH1, runtime::RUNTIME, soc};

#[unsafe(no_mangle)]
extern "C" fn rust_trap(frame: &mut arch::TrapFrame) -> *const arch::TrapFrame {
    let hart = arch::hart_id();
    // Mark this hart as inside M-mode BEFORE acquiring KERNEL_LOCK so a remote
    // hart performing a synchronous PMP shootdown while holding KERNEL_LOCK
    // observes that U-mode execution has stopped without deadlocking.
    soc::smp::mark_hart_in_kernel(hart);
    let entry_cycles = arch::mcycle();
    let interrupt = frame.mcause >> 31 != 0;
    let cause = frame.mcause & 0xfff;
    let from_idle = frame.mstatus & (3 << 11) != 0;
    // Save live PIE enable state (CSR 0x7f2) and disable PIE during M-mode execution.
    let saved_pie_state = if !from_idle {
        unsafe { arch::enter_kernel_pie_off() }
    } else {
        arch::PIE_STATE_OFF
    };
    // A kernel fault during boot may happen before RUNTIME is initialized.
    // Reject all M-mode entries except the known assembly idle IRQ first.
    if from_idle {
        let idle_pc = arch::vela_idle_entry as *const () as u32;
        if !interrupt
            || hart >= HARTS
            || !matches!(
                cause,
                17 | soc::console::RX_LINE
                    | soc::dma::LINE
                    | soc::sentinel::LINE
                    | soc::smp::IPI_LINE
            )
            || frame.mepc.wrapping_sub(idle_pc) >= 8
        {
            fatal(frame);
        }
    }
    if hart >= HARTS {
        fatal(frame);
    }
    soc::smp::KERNEL_LOCK.lock();
    // SAFETY: U-mode and the idle loop are entered only after initialization.
    // KERNEL_LOCK serializes HP0 and HP1; no Rust references survive mret.
    let runtime = unsafe { (*RUNTIME.0.get()).assume_init_mut() };
    runtime.scheduler.enter_hart(hart);
    if from_idle && !runtime.harts[hart].in_idle {
        soc::smp::KERNEL_LOCK.unlock();
        fatal(frame);
    }
    let user_delta = runtime.harts[hart]
        .cycles
        .on_trap_entry(entry_cycles, from_idle);
    if !from_idle {
        if let Some(cur) = runtime.scheduler.hart_current[hart] {
            let updated = runtime
                .scheduler
                .task(cur)
                .user_cycles
                .saturating_add(user_delta);
            runtime.scheduler.task_mut(cur).user_cycles = updated;
        }
    } else if hart == 0 {
        runtime.exit_tickless_idle(interrupt && cause == 17);
    } else if let Some(now_16m) = unsafe { soc::timer::counter_16mhz() } {
        let rates = unsafe { soc::clock::rates_mhz() };
        runtime.harts[hart].cycles.sync_clock(now_16m, rates.cpu);
    }
    let outgoing_thread = if !from_idle {
        runtime.scheduler.hart_current[hart]
    } else {
        None
    };
    let mut switch = false;
    if interrupt && cause == 17 && hart == 0 {
        // SAFETY: sole timer owner under masked IRQs and KERNEL_LOCK. Only bounded O(TASKS)
        // wakeup/accounting, no USB polling or console formatting in timer ISR.
        let raw_16m = unsafe {
            soc::timer::acknowledge();
            soc::timer::counter_16mhz().expect("SYSTIMER snapshot timeout")
        };
        if !from_idle {
            let live_tick = runtime.scheduler.snapshot().scheduler_ticks > 0;
            runtime.harts[0]
                .cycles
                .on_timer_irq(raw_16m, soc::timer::PERIOD_10MS_16M, live_tick);
        }
        if soc::smp::hp1_online() {
            let rates = unsafe { soc::clock::rates_mhz() };
            runtime.harts[1].cycles.sync_clock(raw_16m, rates.cpu);
        }
        let uptime = Ticks(raw_16m / soc::timer::PERIOD_10MS_16M);
        if runtime.dma.timed_out(DMA_CH_GDMA_AHB_CH1, uptime) {
            unsafe {
                soc::dma::finish(runtime.dma.length(DMA_CH_GDMA_AHB_CH1));
            }
            runtime.complete_dma(false);
        }
        runtime.display.tick();
        runtime.sentinel.tick(uptime);
        runtime.scheduler.tick(uptime, &mut runtime.calls);
        runtime.notify_remote_hart(true);
        switch = true;
    } else if interrupt && cause == soc::smp::IPI_LINE {
        soc::smp::acknowledge_ipi(hart);
        runtime.harts[hart].ipi_interrupts += 1;
        switch = from_idle
            || runtime
                .scheduler
                .needs_reschedule_on(hart, hart == 1 && !from_idle);
    } else if interrupt && cause == soc::sentinel::LINE && hart == 0 {
        runtime.sentinel_interrupts += 1;
        if let Some(reply) = unsafe { soc::sentinel::acknowledge(runtime.sentinel.epoch()) } {
            runtime
                .sentinel
                .handle_reply(reply, &mut runtime.scheduler, &runtime.processes);
        }
        runtime.notify_remote_hart(false);
        switch = from_idle || runtime.scheduler.should_preempt_for(hart);
    } else if interrupt && cause == soc::dma::LINE && hart == 0 {
        runtime.dma_interrupts += 1;
        let ok = unsafe { soc::dma::finish(runtime.dma.length(DMA_CH_GDMA_AHB_CH1)) };
        runtime.complete_dma(ok);
        switch = from_idle || runtime.scheduler.should_preempt_for(hart);
    } else if interrupt && let Some(index) = runtime.events.index_by_line(cause) {
        runtime.mask_irq_slot(index);
        if runtime
            .events
            .notify(index, &mut runtime.scheduler)
            .is_some()
        {
            runtime.notify_remote_hart(false);
            switch = from_idle || runtime.scheduler.should_preempt_for(hart);
        } else {
            switch = from_idle;
        }
    } else if !interrupt && cause == 8 && !from_idle {
        frame.mepc = frame.mepc.wrapping_add(4);
        let result = runtime.dispatch(frame, &mut switch);
        frame.set_return(result);
        runtime.notify_remote_hart(false);
    } else if !interrupt && !from_idle {
        let _ = writeln!(
            runtime.console,
            "[fault] hart={} task={} cause={:#x} pc={:#x} value={:#x}",
            hart,
            runtime.scheduler.current().id.raw(),
            frame.mcause,
            frame.mepc,
            frame.mtval
        );
        runtime.terminate();
        runtime.notify_remote_hart(false);
        switch = true;
    } else {
        soc::smp::KERNEL_LOCK.unlock();
        fatal(frame);
    }
    // Preserve active trap bookkeeping, canonicalize target privilege and MPIL.
    let cause = frame.mcause & !((3 << 28) | (255 << 16));
    if !from_idle && !switch {
        let process = runtime.scheduler.current().process;
        // SAFETY: local hart M-mode trap exit after boot PMP initialization.
        if !unsafe { runtime.ensure_active_space_on(hart, process) } {
            soc::smp::KERNEL_LOCK.unlock();
            fatal(frame);
        }
        frame.mcause = cause;
        if (frame.mstatus & arch::MSTATUS_FS_MASK) == 0 {
            frame.mstatus |= arch::MSTATUS_FS_CLEAN;
        }
        unsafe { arch::exit_kernel_pie_restore(saved_pie_state) };
        runtime.harts[hart].cycles.on_trap_exit(arch::mcycle());
        soc::smp::mark_hart_in_user(hart);
        soc::smp::KERNEL_LOCK.unlock();
        return frame;
    }
    if !from_idle {
        if let Some(out_slot) = outgoing_thread {
            unsafe {
                arch::save_extended_context(
                    &mut runtime.ext_contexts[out_slot.index()],
                    saved_pie_state,
                );
            }
        }
        frame.mstatus = (frame.mstatus & !arch::MSTATUS_FS_MASK) | arch::MSTATUS_FS_CLEAN;
        runtime.scheduler.save(*frame);
    }
    let mut selected = runtime.scheduler.select_for(hart, interrupt);
    if selected.is_none() && runtime.poll_pending_before_idle() {
        selected = runtime.scheduler.select_for(hart, false);
    }
    if selected.is_none() {
        runtime.harts[hart].in_idle = true;
        if hart == 0 {
            runtime.enter_tickless_idle();
        }
        runtime.harts[hart].idle_frame.mcause = cause | (3 << 28) | (1 << 27);
        if hart == 0 && !runtime.logs.idle {
            runtime.logs.idle = true;
            let _ = writeln!(runtime.console, "[idle] wfi (no runnable tasks)");
        }
        unsafe { arch::disable_pie() };
        let idle_ptr: *const arch::TrapFrame = &raw const runtime.harts[hart].idle_frame;
        runtime.harts[hart].cycles.on_trap_exit(arch::mcycle());
        soc::smp::KERNEL_LOCK.unlock();
        return idle_ptr;
    }
    runtime.harts[hart].in_idle = false;
    let current_thread = runtime.scheduler.current;
    unsafe {
        arch::restore_extended_context(&runtime.ext_contexts[current_thread.index()]);
    }
    let task = runtime.scheduler.task_mut(current_thread);
    task.context.mcause = cause;
    task.context.mstatus = (task.context.mstatus & !arch::MSTATUS_FS_MASK) | arch::MSTATUS_FS_CLEAN;
    let process = task.process;
    // SAFETY: activate only selected task's RX code / RW stack before mret.
    if !unsafe { runtime.ensure_active_space_on(hart, process) } {
        soc::smp::KERNEL_LOCK.unlock();
        fatal(frame);
    }
    let ctx_ptr: *const arch::TrapFrame = &raw const runtime.scheduler.task(current_thread).context;
    runtime.harts[hart].cycles.on_trap_exit(arch::mcycle());
    soc::smp::mark_hart_in_user(hart);
    soc::smp::KERNEL_LOCK.unlock();
    ctx_ptr
}

fn fatal(frame: &arch::TrapFrame) -> ! {
    // SAFETY: fatal takeover never resumes an interrupted console owner.
    unsafe {
        core::arch::asm!("csrci mstatus,8", options(nostack));
    }
    let mut console = unsafe { soc::console::Console::new() };
    let _ = writeln!(
        console,
        "[kernel trap] hart={} cause={:#x} pc={:#x} value={:#x}",
        arch::hart_id(),
        frame.mcause,
        frame.mepc,
        frame.mtval
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
    let _ = writeln!(console, "[panic] hart={} {info}", arch::hart_id());
    arch::halt()
}
