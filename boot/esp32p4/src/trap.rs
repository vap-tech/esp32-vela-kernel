use core::fmt::Write;
use vela_kernel::{
    errno::Errno,
    id::{ThreadSlot, Ticks},
    sched::TASKS,
    task::{State, Wait},
};

use crate::{
    arch, pmp,
    runtime::{CONSOLE_RX, RUNTIME},
    soc,
};

#[unsafe(no_mangle)]
extern "C" fn rust_trap(frame: &mut arch::TrapFrame) -> *const arch::TrapFrame {
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
    let mut switch = false;
    if interrupt && cause == 17 {
        // SAFETY: sole timer owner under masked IRQs. Only bounded O(TASKS)
        // wakeup/accounting, no USB polling or console formatting in timer ISR.
        unsafe {
            soc::timer::acknowledge();
        }
        let uptime =
            Ticks(unsafe { soc::timer::uptime_ticks() }.expect("SYSTIMER snapshot timeout"));
        if matches!(
            runtime.dma,
            vela_kernel::dma::Lease::Device(_) | vela_kernel::dma::Lease::Detached
        ) && uptime >= runtime.dma_deadline
        {
            unsafe {
                soc::dma::finish(runtime.dma_length);
            }
            runtime.complete_dma(false);
        }
        if runtime.sentinel_pending {
            for i in 0..TASKS {
                let thread = ThreadSlot(i);
                if matches!(runtime.scheduler.task(thread).wait, Wait::Sentinel { deadline, .. } if uptime >= deadline)
                {
                    runtime.scheduler.task_mut(thread).context.registers[8] =
                        Errno::Etimedout.to_abi();
                    runtime.scheduler.wake(thread);
                }
            }
        }
        for i in 0..TASKS {
            let thread = ThreadSlot(i);
            if runtime.scheduler.task(thread).state == State::Blocked
                && matches!(runtime.scheduler.task(thread).wait, Wait::SentinelWake { deadline, .. } if uptime >= deadline)
            {
                runtime.scheduler.task_mut(thread).context.registers[8] = Errno::Etimedout.to_abi();
                runtime.scheduler.task_mut(thread).context.registers[9] = 0;
                runtime.scheduler.wake(thread);
                // Keep the LP token reserved: a late event cannot complete a new alarm.
            }
        }
        soc::scanout::tick();
        runtime.sentinel_tick(uptime);
        runtime.expire_calls(uptime);
        runtime.scheduler.tick(uptime);
        switch = true;
    } else if interrupt && cause == soc::sentinel::LINE {
        if let Some(reply) = unsafe { soc::sentinel::acknowledge(runtime.sentinel_epoch) } {
            runtime.sentinel_reply(reply);
        }
        switch = from_idle || runtime.scheduler.should_preempt();
    } else if interrupt && cause == soc::dma::LINE {
        let ok = unsafe { soc::dma::finish(runtime.dma_length) };
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
        frame.registers[8] = runtime.dispatch(frame, &mut switch);
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
    let current_thread = runtime.scheduler.current;
    let task = runtime.scheduler.task_mut(current_thread);
    task.context.mcause = cause;
    let process = task.process;
    let memory = runtime.processes.slot(process).memory;
    // SAFETY: activate only selected task's RX code / RW stack before mret.
    if runtime.active_space != Some((process, memory.revision)) {
        let image = match runtime.pmp_images[process.index()] {
            Some((revision, image)) if revision == memory.revision => image,
            _ => {
                let image = pmp::Prepared::new(&memory).unwrap();
                runtime.pmp_images[process.index()] = Some((memory.revision, image));
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
