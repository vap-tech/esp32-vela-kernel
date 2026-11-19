use core::fmt::Write;
use vela_kernel::{
    id::{ThreadSlot, Ticks},
    task::{Context, State, Wait},
};

use crate::{
    arch,
    runtime::{CONSOLE_RX, RUNTIME},
    soc,
};

#[unsafe(no_mangle)]
extern "C" fn rust_trap(frame: &mut arch::TrapFrame) -> *const arch::TrapFrame {
    let entry_cycles = arch::mcycle();
    let interrupt = frame.mcause >> 31 != 0;
    let cause = frame.mcause & 0xfff;
    let from_idle = frame.mstatus & (3 << 11) != 0;
    // A kernel fault during boot may happen before RUNTIME is initialized.
    // Reject all M-mode entries except the known assembly idle IRQ first.
    if from_idle {
        let idle_pc = arch::vela_idle_entry as *const () as u32;
        if !interrupt
            || !matches!(
                cause,
                17 | soc::console::RX_LINE | soc::dma::LINE | soc::sentinel::LINE
            )
            || frame.mepc.wrapping_sub(idle_pc) >= 8
        {
            fatal(frame);
        }
    }
    // SAFETY: U-mode and the idle loop are entered only after initialization.
    // No Rust references survive mret. All Rust kernel paths have MIE=0.
    let runtime = unsafe { (*RUNTIME.0.get()).assume_init_mut() };
    if from_idle && !runtime.in_idle {
        fatal(frame);
    }
    let user_delta = runtime.cycles.on_trap_entry(entry_cycles, from_idle);
    if !from_idle {
        let cur = runtime.scheduler.current;
        let updated = runtime
            .scheduler
            .task(cur)
            .user_cycles
            .saturating_add(user_delta);
        runtime.scheduler.task_mut(cur).user_cycles = updated;
    }
    let mut switch = false;
    if interrupt && cause == 17 {
        // SAFETY: sole timer owner under masked IRQs. Only bounded O(TASKS)
        // wakeup/accounting, no USB polling or console formatting in timer ISR.
        let raw_16m = unsafe {
            soc::timer::acknowledge();
            soc::timer::counter_16mhz().expect("SYSTIMER snapshot timeout")
        };
        let live_tick = runtime.scheduler.snapshot().scheduler_ticks > 0;
        runtime.cycles.on_timer_irq(raw_16m, 160_000, live_tick);
        let uptime = Ticks(raw_16m / 160_000);
        if runtime.dma.timed_out(uptime) {
            unsafe {
                soc::dma::finish(runtime.dma.length());
            }
            runtime.complete_dma(false);
        }
        runtime.display.tick();
        runtime.sentinel.tick(uptime);
        runtime.scheduler.tick(uptime, &mut runtime.calls);
        switch = true;
    } else if interrupt && cause == soc::sentinel::LINE {
        runtime.sentinel_interrupts += 1;
        if let Some(reply) = unsafe { soc::sentinel::acknowledge(runtime.sentinel.epoch()) } {
            runtime
                .sentinel
                .handle_reply(reply, &mut runtime.scheduler, &runtime.processes);
        }
        switch = from_idle || runtime.scheduler.should_preempt();
    } else if interrupt && cause == soc::dma::LINE {
        runtime.dma_interrupts += 1;
        let ok = unsafe { soc::dma::finish(runtime.dma.length()) };
        runtime.complete_dma(ok);
        switch = from_idle || runtime.scheduler.should_preempt();
    } else if interrupt && cause == soc::console::RX_LINE {
        runtime.usb_rx_interrupts += 1;
        runtime.console.mask_rx_irq();
        if let Some(i) = runtime.scheduler.tasks.iter().position(|t| {
            t.state == State::Blocked && matches!(t.wait, Wait::Event { object: CONSOLE_RX })
        }) {
            runtime.scheduler.wake(ThreadSlot(i));
            switch = from_idle || runtime.scheduler.should_preempt();
        } else {
            switch = from_idle;
        }
    } else if !interrupt && cause == 8 && !from_idle {
        frame.mepc = frame.mepc.wrapping_add(4);
        let result = runtime.dispatch(frame, &mut switch);
        frame.set_return(result);
    } else if !interrupt && !from_idle {
        let _ = writeln!(
            runtime.console,
            "[fault] task={} cause={:#x} pc={:#x} value={:#x}",
            runtime.scheduler.current().id.raw(),
            frame.mcause,
            frame.mepc,
            frame.mtval
        );
        runtime.terminate();
        switch = true;
    } else {
        fatal(frame);
    }
    // Preserve active trap bookkeeping, canonicalize target privilege and MPIL.
    let cause = frame.mcause & !((3 << 28) | (255 << 16));
    if !from_idle && !switch {
        let process = runtime.scheduler.current().process;
        // SAFETY: sole hart0 M-mode trap exit after boot PMP initialization.
        if !unsafe { runtime.ensure_active_space(process) } {
            fatal(frame);
        }
        frame.mcause = cause;
        runtime.cycles.on_trap_exit(arch::mcycle());
        return frame;
    }
    if !from_idle {
        runtime.scheduler.save(*frame);
    }
    let selected = runtime.scheduler.select(interrupt);
    if selected.is_none() {
        runtime.in_idle = true;
        runtime.idle_frame.mcause = cause | (3 << 28) | (1 << 27);
        if !runtime.logs.idle {
            runtime.logs.idle = true;
            let _ = writeln!(runtime.console, "[idle] wfi (no runnable tasks)");
        }
        runtime.cycles.on_trap_exit(arch::mcycle());
        return &runtime.idle_frame;
    }
    runtime.in_idle = false;
    let current_thread = runtime.scheduler.current;
    let task = runtime.scheduler.task_mut(current_thread);
    task.context.mcause = cause;
    let process = task.process;
    // SAFETY: activate only selected task's RX code / RW stack before mret.
    if !unsafe { runtime.ensure_active_space(process) } {
        fatal(frame);
    }
    runtime.cycles.on_trap_exit(arch::mcycle());
    &runtime.scheduler.task(current_thread).context
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
