use vela_kernel::{
    capability::{self as cap, Authority},
    errno::Errno,
    id::{Handle, Pid, ProcSlot, Tid},
    lifecycle,
    memory::MemoryClass,
    task::{Region, Wait},
};

use super::Args;
use crate::{
    arch, board,
    dma::DMA_CH_GDMA_AHB_CH1,
    runtime::{IRQ_SLOT_CONSOLE_RX, Runtime},
    soc, syscall,
    usermem::{copy_to_user, copy_to_user_within},
};

impl Runtime {
    pub(super) fn sys_spawn_hello(&mut self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        lifecycle::require_authority(
            &self.processes,
            current,
            Handle(args.a0),
            Authority::ProcessSpawn,
            cap::EXECUTE,
        )?;
        let (bytes, class): (&[u8], MemoryClass) = match args.a1 {
            0 => (
                include_bytes!(concat!(env!("OUT_DIR"), "/hello.elf")),
                MemoryClass::Sram,
            ),
            2 => (
                include_bytes!(concat!(env!("OUT_DIR"), "/dma.elf")),
                MemoryClass::Sram,
            ),
            1 => (
                include_bytes!(concat!(env!("OUT_DIR"), "/pic.elf")),
                MemoryClass::Sram,
            ),
            3 => (
                include_bytes!(concat!(env!("OUT_DIR"), "/pic.elf")),
                MemoryClass::Psram,
            ),
            _ => return Err(Errno::Einval),
        };
        self.spawn_elf_in(bytes, class, args.a1 == 2).map(Pid::raw)
    }

    pub(super) fn sys_yield(&mut self, switch: &mut bool) -> Result<u32, Errno> {
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_exit(&mut self, switch: &mut bool) -> Result<u32, Errno> {
        self.terminate();
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_thread_priority(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        lifecycle::set_thread_priority(
            &self.processes,
            &mut self.scheduler,
            current,
            Tid(args.a0),
            args.a1,
        )?;
        *switch = self.scheduler.should_preempt();
        Ok(0)
    }

    pub(super) fn sys_thread_create(
        &mut self,
        current: ProcSlot,
        args: Args,
    ) -> Result<u32, Errno> {
        let stack = Region {
            base: args.a1 as usize,
            size: args.a2 as usize,
        };
        lifecycle::spawn_user_thread(
            &self.processes,
            &self.allocator,
            &mut self.scheduler,
            current,
            args.a0 as usize,
            stack,
            args.a4,
            args.a5,
            |entry, sp| arch::TrapFrame::user(entry, sp, args.a3),
        )
        .map(Tid::raw)
    }

    pub(super) fn sys_process_exit(
        &mut self,
        current: ProcSlot,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let owner = self.processes.slot(current).id;
        let display = &mut self.display;
        self.allocator.release_psram_owner_with(owner, |region| {
            let _ = display.set_psram_region_mapped(region, false);
        });
        let reaped = lifecycle::terminate_process(
            &mut self.processes,
            &mut self.ipc,
            &mut self.calls,
            &mut self.events,
            &mut self.allocator,
            &mut self.scheduler,
            copy_to_user_within,
        );
        self.finish_reaped_process(reaped);
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_ticks(&self) -> Result<u32, Errno> {
        Ok(self.scheduler.uptime().raw() as u32)
    }

    pub(super) fn sys_stats(&mut self, args: Args) -> Result<u32, Errno> {
        let ptr = args.a0 as usize;
        let len = args.a1 as usize;
        let usb_rx_interrupts = self.events.interrupts(IRQ_SLOT_CONSOLE_RX);
        match args.a2 {
            syscall::STATS_SUMMARY => {
                if !matches!(len, syscall::STATS_BYTES | syscall::STATS_IRQ_BYTES) {
                    return Err(Errno::Einval);
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
                    usb_rx_interrupts as u32,
                    (usb_rx_interrupts >> 32) as u32,
                ];
                let mut buffer = [0u8; syscall::STATS_IRQ_BYTES];
                for (i, word) in words[..len / 4].iter().enumerate() {
                    buffer[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
                }
                copy_to_user(self.memory(), ptr, &buffer[..len])
            }
            syscall::STATS_CPU => {
                if len != syscall::STATS_CPU_BYTES {
                    return Err(Errno::Einval);
                }
                // SAFETY: sole HP0 owner under masked IRQs.
                let rates = unsafe { crate::soc::clock::rates_mhz() };
                let now_16m =
                    unsafe { crate::soc::timer::counter_16mhz() }.expect("SYSTIMER stats snapshot");
                self.cycles.sync_clock(now_16m, rates.cpu);
                let cyc = self.cycles.snapshot(arch::mcycle(), now_16m);
                let words = [
                    arch::hart_id() as u32,
                    rates.cpu,
                    rates.apb,
                    self.sentinel.hz(),
                    cyc.user_cycles as u32,
                    (cyc.user_cycles >> 32) as u32,
                    cyc.kernel_cycles as u32,
                    (cyc.kernel_cycles >> 32) as u32,
                    cyc.idle_cycles as u32,
                    (cyc.idle_cycles >> 32) as u32,
                    cyc.capacity_cycles as u32,
                    (cyc.capacity_cycles >> 32) as u32,
                    cyc.irq_latency_last_16m,
                    cyc.irq_latency_max_16m,
                    self.freq_mode.to_abi() | (cyc.freq_switches << 8),
                    cyc.tickless_entries,
                    cyc.tickless_ticks as u32,
                    (cyc.tickless_ticks >> 32) as u32,
                ];
                let mut buffer = [0u8; syscall::STATS_CPU_BYTES];
                for (i, word) in words.into_iter().enumerate() {
                    buffer[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
                }
                copy_to_user(self.memory(), ptr, &buffer)
            }
            syscall::STATS_IRQ => {
                if len != syscall::STATS_IRQ_ALL_BYTES {
                    return Err(Errno::Einval);
                }
                let systimer = self.scheduler.snapshot().scheduler_ticks;
                let counts = [
                    systimer,
                    usb_rx_interrupts,
                    self.dma_interrupts,
                    self.sentinel_interrupts,
                ];
                let mut buffer = [0u8; syscall::STATS_IRQ_ALL_BYTES];
                for (i, count) in counts.into_iter().enumerate() {
                    buffer[i * 8..i * 8 + 8].copy_from_slice(&count.to_le_bytes());
                }
                copy_to_user(self.memory(), ptr, &buffer)
            }
            syscall::STATS_STREAMS => {
                if len != syscall::STATS_STREAMS_BYTES {
                    return Err(Errno::Einval);
                }
                let scan = self.display.telemetry();
                let dma_bytes = self.dma.bytes(DMA_CH_GDMA_AHB_CH1);
                let words = [
                    (u32::from(scan.mode) << 8) | u32::from(scan.running),
                    scan.frames,
                    scan.underruns,
                    scan.dma_errors,
                    scan.fifo_level,
                    self.dma.completed(DMA_CH_GDMA_AHB_CH1),
                    self.dma.errors(DMA_CH_GDMA_AHB_CH1),
                    dma_bytes as u32,
                    (dma_bytes >> 32) as u32,
                ];
                let mut buffer = [0u8; syscall::STATS_STREAMS_BYTES];
                for (i, word) in words.into_iter().enumerate() {
                    buffer[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
                }
                copy_to_user(self.memory(), ptr, &buffer)
            }
            syscall::STATS_TASKS => {
                if len != syscall::STATS_TASKS_BYTES {
                    return Err(Errno::Einval);
                }
                let mut buffer = [0u8; syscall::STATS_TASKS_BYTES];
                for (slot, task) in self.scheduler.tasks.iter().enumerate() {
                    let base = slot * syscall::STATS_TASK_WORDS * 4;
                    let code = task.state_code();
                    if code == 0 {
                        continue;
                    }
                    let pid = self.processes.slot(task.process).id.raw();
                    let meta = u32::from(code)
                        | (u32::from(task.priority) << 8)
                        | (u32::from(task.effective_priority) << 16)
                        | (u32::from(task.affinity) << 24);
                    let words = [
                        task.id.raw(),
                        pid,
                        meta,
                        task.stack.size as u32,
                        task.preemptions.min(u64::from(u32::MAX)) as u32,
                        task.runtime_ticks as u32,
                        (task.runtime_ticks >> 32) as u32,
                        task.user_cycles as u32,
                        (task.user_cycles >> 32) as u32,
                    ];
                    for (i, word) in words.into_iter().enumerate() {
                        buffer[base + i * 4..base + i * 4 + 4].copy_from_slice(&word.to_le_bytes());
                    }
                }
                copy_to_user(self.memory(), ptr, &buffer)
            }
            syscall::STATS_SDCARD => {
                if len != syscall::STATS_SDCARD_BYTES {
                    return Err(Errno::Einval);
                }
                // SAFETY: sole HP0 owner under masked IRQs.
                let sd = unsafe { soc::sdcard::telemetry() };
                let asset = unsafe { board::images::status() };
                let flags = u32::from(sd.ready)
                    | (u32::from(sd.sdhc) << 1)
                    | (u32::from(asset.verified) << 2)
                    | (u32::from(asset.provisioned) << 3);
                let words = [
                    flags,
                    sd.ocr,
                    sd.sectors,
                    asset.asset_lba,
                    asset.asset_bytes,
                    sd.read_sectors,
                    sd.written_sectors,
                    sd.errors,
                ];
                let mut buffer = [0u8; syscall::STATS_SDCARD_BYTES];
                for (i, word) in words.into_iter().enumerate() {
                    buffer[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
                }
                copy_to_user(self.memory(), ptr, &buffer)
            }
            _ => Err(Errno::Einval),
        }?;
        Ok(0)
    }

    pub(super) fn sys_sleep(&mut self, args: Args, switch: &mut bool) -> Result<u32, Errno> {
        if args.a0 == 0 || args.a0 > 6000 {
            return Err(Errno::Einval);
        }
        let until = self.scheduler.uptime().saturating_add(u64::from(args.a0));
        self.scheduler.block_until(Wait::Sleep, until);
        *switch = true;
        Ok(0)
    }
}
