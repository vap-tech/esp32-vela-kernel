use vela_kernel::{
    capability::{self as cap, Authority, Capability, Object},
    errno::Errno,
    id::{Handle, ProcSlot, ThreadSlot, Tid},
    lifecycle,
    memory::MemoryClass,
    task::{Context, Region, State, Wait},
};

use super::Args;
use crate::{
    arch, board,
    dma::DMA_CH_GDMA_AHB_CH1,
    runtime::{IRQ_SLOT_CONSOLE_RX, IRQ_SLOT_KBD, Runtime},
    soc, syscall,
    usermem::{copy_from_user, copy_to_user, copy_to_user_within},
};

const _: () = {
    assert!(syscall::CAP_READ == cap::READ as u32);
    assert!(syscall::CAP_WRITE == cap::WRITE as u32);
    assert!(syscall::CAP_EXECUTE == cap::EXECUTE as u32);
    assert!(syscall::CAP_SEND == cap::SEND as u32);
    assert!(syscall::CAP_RECEIVE == cap::RECEIVE as u32);
    assert!(syscall::CAP_GRANT_RIGHT == cap::GRANT as u32);
    assert!(syscall::CAP_WAIT == cap::WAIT as u32);
    assert!(syscall::CAP_ACK == cap::ACK as u32);
    assert!(syscall::CAP_CONTROL == cap::CONTROL as u32);
    assert!(syscall::CAP_PASS_ON_SPAWN == cap::PASS_ON_SPAWN as u32);
};

impl Runtime {
    pub(super) fn sys_kbd_supervise(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let supervisor = self
            .processes
            .slot(current)
            .capabilities
            .get(Handle(args.a0), cap::CONTROL)?;
        let Object::Authority(Authority::KeyboardSupervisor(input)) = supervisor.object else {
            return Err(Errno::Ebadf);
        };
        let pid = if let Some(pid) = self.kbd_pid {
            pid
        } else {
            let (child, _, _) = self.load_elf(crate::boot::KBD_ELF, false)?;
            let result = (|| {
                let sender =
                    lifecycle::grant(&mut self.processes, current, input, child.slot, cap::SEND)?;
                let bus = self
                    .processes
                    .slot_mut(child.slot)
                    .capabilities
                    .install(Capability {
                        object: Object::Authority(Authority::I2c(syscall::I2C_BUS_EXT1)),
                        rights: cap::READ | cap::WRITE,
                    })?;
                let irq = lifecycle::bind_event(
                    &mut self.events,
                    &mut self.processes,
                    &mut self.scheduler,
                    child.slot,
                    IRQ_SLOT_KBD,
                    cap::READ | cap::WAIT | cap::ACK,
                )?;
                self.start_loaded(child, [sender.raw(), bus.raw(), irq.raw(), 0, 0, 0], 2, 2)
            })();
            if let Err(error) = result {
                self.events.remove_process(child.slot, &mut self.scheduler);
                self.abort_loaded(child);
                return Err(error);
            }
            self.kbd_pid = Some(child.pid);
            child.pid
        };
        self.scheduler.block(Wait::ProcessExit { pid });
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_fbcon_supervise(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        lifecycle::require_authority(
            &self.processes,
            current,
            Handle(args.a0),
            Authority::FbconSupervisor,
            cap::CONTROL,
        )?;
        let pid = if let Some(pid) = self.fbcon_pid {
            pid
        } else {
            let (child, _, _) =
                self.load_elf_in(crate::boot::FBCON_ELF, MemoryClass::Psram, false)?;
            let result = (|| {
                let caps = &mut self.processes.slot_mut(child.slot).capabilities;
                let display = caps.install(Capability {
                    object: Object::Authority(Authority::Display),
                    rights: cap::CONTROL,
                })?;
                let log = caps.install(Capability {
                    object: Object::Authority(Authority::ConsoleLog),
                    rights: cap::READ,
                })?;
                self.start_loaded(child, [display.raw(), log.raw(), 0, 0, 0, 0], 1, 1)
            })();
            if let Err(error) = result {
                self.abort_loaded(child);
                return Err(error);
            }
            self.fbcon_pid = Some(child.pid);
            child.pid
        };
        self.scheduler.block(Wait::ProcessExit { pid });
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_spawn_image(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let spawn_cap = self
            .processes
            .slot(current)
            .capabilities
            .get(Handle(args.a0), 0)?;
        if spawn_cap.object != Object::Authority(Authority::ProcessSpawn) {
            return Err(Errno::Ebadf);
        }
        if spawn_cap.rights & cap::EXECUTE != cap::EXECUTE {
            return Err(Errno::Eperm);
        }
        let allow_repass = (spawn_cap.rights & cap::CONTROL) != 0;

        let len = args.a2 as usize;
        let ptr = args.a1 as usize;
        if len == 0 || len > syscall::SPAWN_IMAGE_MAX {
            return Err(Errno::Emsgsize);
        }
        let arg_ptr = args.a3 as usize;
        let arg_len = args.a4 as usize;
        if arg_len > syscall::SPAWN_ARG_MAX {
            return Err(Errno::Emsgsize);
        }
        let caps_ptr = args.a5 as usize;
        let caps_bytes = core::mem::size_of::<syscall::SpawnCaps>();
        if self.scheduler.tasks.iter().enumerate().any(|(i, task)| {
            ThreadSlot(i) != self.scheduler.current
                && task.process == current
                && task.state == State::Running
        }) {
            return Err(Errno::Ebusy);
        }
        self.check_private_spawn_input(current, ptr, len)?;
        self.check_private_spawn_input(current, arg_ptr, arg_len)?;
        if caps_ptr != 0 {
            self.check_private_spawn_input(current, caps_ptr, caps_bytes)?;
        }

        let mut inherited: [Option<Capability>; syscall::SPAWN_CAPS_MAX] =
            [None; syscall::SPAWN_CAPS_MAX];
        if caps_ptr != 0 {
            let mut raw = [0u8; core::mem::size_of::<syscall::SpawnCaps>()];
            copy_from_user(self.memory(), caps_ptr, &mut raw)?;
            let count = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
            if count > syscall::SPAWN_CAPS_MAX {
                return Err(Errno::Einval);
            }
            for (i, slot) in inherited.iter_mut().take(count).enumerate() {
                let base = 4 + i * 8;
                let handle =
                    u32::from_le_bytes([raw[base], raw[base + 1], raw[base + 2], raw[base + 3]]);
                let rights = u32::from_le_bytes([
                    raw[base + 4],
                    raw[base + 5],
                    raw[base + 6],
                    raw[base + 7],
                ]);
                if handle == 0 {
                    continue;
                }
                let rights = u16::try_from(rights).map_err(|_| Errno::Eperm)?;
                *slot = Some(lifecycle::capability_for_spawn(
                    &self.processes,
                    &self.ipc,
                    current,
                    allow_repass,
                    Handle(handle),
                    rights,
                )?);
            }
        }

        // Verify the image slice is wholly mapped readable in the caller's address
        // space. Because KERNEL_LOCK is held with MIE=0, no sibling thread is running,
        // and `check_private_spawn_input` rejected any foreign writable alias, the
        // image cannot mutate while `load_elf_in` parses and copies it into a newly
        // allocated child PSRAM region.
        if !self.readable(ptr, len) {
            return Err(Errno::Efault);
        }
        let mut argument = [0; syscall::SPAWN_ARG_MAX];
        if arg_len != 0 {
            copy_from_user(self.memory(), arg_ptr, &mut argument[..arg_len])?;
        }
        // SAFETY: `ptr..ptr+len` is a single contiguous readable mapping in SRAM or
        // mapped PSRAM with no concurrent writer; `load_elf_in` allocates disjoint
        // memory for the child and does not retain the slice.
        let image_bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len) };
        let (loaded, _, _) = self.load_elf_in(image_bytes, MemoryClass::Psram, false)?;
        let result = (|| {
            let mut handles = [0u32; syscall::SPAWN_CAPS_MAX];
            for (dst, cap_opt) in handles.iter_mut().zip(inherited) {
                if let Some(capability) = cap_opt {
                    *dst = self
                        .processes
                        .slot_mut(loaded.slot)
                        .capabilities
                        .install(capability)?
                        .raw();
                    if capability.rights & cap::SEND != 0
                        && let Ok(ep) = self.ipc.resolve(capability.object)
                    {
                        self.ipc.mark_sender_attached(ep);
                    }
                }
            }
            let top = loaded.stack.base + loaded.stack.size;
            let sp = (top - arg_len) & !15;
            let child_arg = if arg_len == 0 { 0 } else { sp as u32 };
            if arg_len != 0 {
                // SAFETY: the child hasn't started. Reserve the argument above
                // its initial SP so user stack growth cannot overwrite it.
                unsafe {
                    core::ptr::copy_nonoverlapping(argument.as_ptr(), sp as *mut u8, arg_len)
                };
            }
            self.start_loaded_at(
                loaded,
                sp,
                [
                    handles[0],
                    child_arg,
                    arg_len as u32,
                    handles[1],
                    handles[2],
                    handles[3],
                ],
                1,
                1,
            )?;
            Ok(loaded.pid.raw())
        })();
        if result.is_err() {
            self.abort_loaded(loaded);
        } else {
            if args.number == syscall::SPAWN_IMAGE_WAIT {
                // The child cannot run until we leave the kernel lock, so the
                // wait is registered before exit (including a fault) is possible.
                self.scheduler.block(Wait::ProcessExit { pid: loaded.pid });
                *switch = true;
            }
            self.notify_remote_hart(false);
        }
        result
    }

    fn check_private_spawn_input(
        &self,
        current: ProcSlot,
        ptr: usize,
        len: usize,
    ) -> Result<(), Errno> {
        if len == 0 {
            return Ok(());
        }
        let end = ptr.checked_add(len).ok_or(Errno::Efault)?;
        if self.processes.slots.iter().enumerate().any(|(i, process)| {
            ProcSlot(i) != current
                && process.memory.maps.iter().any(|map| {
                    map.permissions & vela_kernel::process::WRITE != 0
                        && map.region.size != 0
                        && map.region.base < end
                        && ptr < map.region.base.saturating_add(map.region.size)
                })
        }) {
            return Err(Errno::Ebusy);
        }
        Ok(())
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

    pub(super) fn sys_thread_affinity(
        &mut self,
        current: ProcSlot,
        args: Args,
        frame: &mut arch::TrapFrame,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let (thread, affinity) =
            lifecycle::set_thread_affinity(&mut self.scheduler, current, Tid(args.a0), args.a1)?;
        let hart = self.scheduler.active_hart;
        if args.a1 != 0 {
            if thread == self.scheduler.current && (affinity & (1u8 << hart)) == 0 {
                *switch = true;
            } else {
                *switch = self.scheduler.should_preempt_for(hart);
            }
            self.notify_remote_hart(false);
        }
        let running_hart = if thread == self.scheduler.current && (affinity & (1u8 << hart)) == 0 {
            affinity.trailing_zeros() as usize
        } else {
            self.scheduler
                .running_hart(thread)
                .unwrap_or_else(|| self.scheduler.task(thread).last_hart as usize)
        };
        frame.set_return2(running_hart as u32);
        Ok(u32::from(affinity))
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
        let tid = lifecycle::spawn_user_thread(
            &self.processes,
            &self.allocator,
            &mut self.scheduler,
            current,
            args.a0 as usize,
            stack,
            args.a4,
            args.a5,
            |entry, sp| arch::TrapFrame::user(entry, sp, args.a3),
        )?;
        if let Some(slot) = self.scheduler.find_by_id(tid) {
            self.ext_contexts[slot.index()] = arch::ExtendedContext::ZERO;
        }
        self.notify_remote_hart(false);
        Ok(tid.raw())
    }

    pub(super) fn sys_process_exit(&mut self, switch: &mut bool) -> Result<u32, Errno> {
        let local = self.scheduler.active_hart;
        let harts = &mut self.harts;
        let dma = &mut self.dma;
        let display = &mut self.display;
        let reaped = lifecycle::terminate_process(
            &mut self.processes,
            &mut self.ipc,
            &mut self.calls,
            &mut self.events,
            &mut self.allocator,
            &mut self.scheduler,
            |processes, allocator, owner| {
                Self::sync_remote_pmp(processes, harts, local);
                display.detach_owner(owner, allocator, dma);
                allocator.release_psram_owner_with(owner, |region| {
                    let _ = display.set_psram_region_mapped(region, false);
                });
            },
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
                if !matches!(len, syscall::STATS_CPU_BYTES | syscall::STATS_CPU_SMP_BYTES) {
                    return Err(Errno::Einval);
                }
                // SAFETY: serialized by KERNEL_LOCK with IRQs masked.
                let rates = unsafe { crate::soc::clock::rates_mhz() };
                let now_16m =
                    unsafe { crate::soc::timer::counter_16mhz() }.expect("SYSTIMER stats snapshot");
                let local_hart = arch::hart_id().min(1);
                let local_mcycle = arch::mcycle();
                for h in 0..2 {
                    self.harts[h].cycles.sync_clock(now_16m, rates.cpu);
                }
                let cyc0 = if local_hart == 0 {
                    self.harts[0].cycles.snapshot(local_mcycle, now_16m)
                } else {
                    self.harts[0].cycles.snapshot_remote(now_16m)
                };
                let cyc1 = if local_hart == 1 {
                    self.harts[1].cycles.snapshot(local_mcycle, now_16m)
                } else {
                    self.harts[1].cycles.snapshot_remote(now_16m)
                };
                let online_harts = 1 + u32::from(soc::smp::hp1_online());
                let ipi_total = self.harts[0]
                    .ipi_interrupts
                    .saturating_add(self.harts[1].ipi_interrupts);
                let shootdowns = self.harts[0]
                    .pmp_shootdowns
                    .saturating_add(self.harts[1].pmp_shootdowns);
                let words = [
                    arch::hart_id() as u32,
                    rates.cpu,
                    rates.apb,
                    self.sentinel.hz(),
                    cyc0.user_cycles as u32,
                    (cyc0.user_cycles >> 32) as u32,
                    cyc0.kernel_cycles as u32,
                    (cyc0.kernel_cycles >> 32) as u32,
                    cyc0.idle_cycles as u32,
                    (cyc0.idle_cycles >> 32) as u32,
                    cyc0.capacity_cycles as u32,
                    (cyc0.capacity_cycles >> 32) as u32,
                    cyc0.irq_latency_last_16m,
                    cyc0.irq_latency_max_16m,
                    self.freq_mode.to_abi() | (cyc0.freq_switches << 8),
                    cyc0.tickless_entries,
                    cyc0.tickless_ticks as u32,
                    (cyc0.tickless_ticks >> 32) as u32,
                    // Extended SMP words (words 18..31 = bytes 72..128):
                    online_harts,
                    u32::from(soc::smp::hp1_online()),
                    cyc1.user_cycles as u32,
                    (cyc1.user_cycles >> 32) as u32,
                    cyc1.kernel_cycles as u32,
                    (cyc1.kernel_cycles >> 32) as u32,
                    cyc1.idle_cycles as u32,
                    (cyc1.idle_cycles >> 32) as u32,
                    cyc1.capacity_cycles as u32,
                    (cyc1.capacity_cycles >> 32) as u32,
                    ipi_total as u32,
                    (ipi_total >> 32) as u32,
                    shootdowns as u32,
                    (shootdowns >> 32) as u32,
                ];
                let mut buffer = [0u8; syscall::STATS_CPU_SMP_BYTES];
                for (i, word) in words[..len / 4].iter().enumerate() {
                    buffer[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
                }
                copy_to_user(self.memory(), ptr, &buffer[..len])
            }
            syscall::STATS_IRQ => {
                if !matches!(
                    len,
                    syscall::STATS_IRQ_ALL_BYTES
                        | syscall::STATS_IRQ_SMP_BYTES
                        | syscall::STATS_IRQ_FULL_BYTES
                ) {
                    return Err(Errno::Einval);
                }
                let systimer = self.scheduler.snapshot().scheduler_ticks;
                let shootdowns = self.harts[0]
                    .pmp_shootdowns
                    .saturating_add(self.harts[1].pmp_shootdowns);
                let (kbd_arm_low, kbd_arm_wait) = soc::gpio_irq::arm_counts();
                let counts = [
                    systimer,
                    usb_rx_interrupts,
                    self.dma_interrupts,
                    self.sentinel_interrupts,
                    self.harts[0].ipi_interrupts,
                    self.harts[1].ipi_interrupts,
                    shootdowns,
                    self.events.interrupts(IRQ_SLOT_KBD),
                    kbd_arm_low,
                    kbd_arm_wait,
                ];
                let mut buffer = [0u8; syscall::STATS_IRQ_FULL_BYTES];
                for (i, count) in counts[..len / 8].iter().enumerate() {
                    buffer[i * 8..i * 8 + 8].copy_from_slice(&count.to_le_bytes());
                }
                copy_to_user(self.memory(), ptr, &buffer[..len])
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
            syscall::STATS_DISPLAY => {
                if len != syscall::STATS_DISPLAY_BYTES {
                    return Err(Errno::Einval);
                }
                let mut flags = 0;
                if self.display.blanked() {
                    flags |= syscall::DISPLAY_FLAG_BLANKED;
                }
                if self.sd_export_owner.is_some() {
                    flags |= syscall::DISPLAY_FLAG_SD_EXPORT;
                    if self.usb_storage.writable() {
                        flags |= syscall::DISPLAY_FLAG_SD_EXPORT_RW;
                    }
                }
                copy_to_user(self.memory(), ptr, &flags.to_le_bytes())
            }
            syscall::STATS_TASKS => {
                if len != 8 * syscall::STATS_TASK_WORDS * 4 && len != syscall::STATS_TASKS_BYTES {
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
                    let hart = self
                        .scheduler
                        .running_hart(ThreadSlot(slot))
                        .unwrap_or(task.last_hart as usize)
                        & 1;
                    // meta[24..26] = affinity (1..3), meta[26] = active/last hart (0 or 1),
                    // meta[28..32] = migrations & 0xf.
                    let packed_smp = u32::from(task.affinity & 3)
                        | ((hart as u32) << 2)
                        | ((task.migrations.min(15)) << 4);
                    let meta = u32::from(code)
                        | (u32::from(task.priority) << 8)
                        | (u32::from(task.effective_priority) << 16)
                        | (packed_smp << 24);
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
                copy_to_user(self.memory(), ptr, &buffer[..len])
            }
            syscall::STATS_SDCARD => {
                if !matches!(
                    len,
                    syscall::STATS_SDCARD_BYTES | syscall::STATS_SDCARD_EXT_BYTES
                ) {
                    return Err(Errno::Einval);
                }
                // SAFETY: serialized by KERNEL_LOCK with IRQs masked.
                let sd = unsafe { soc::sdcard::telemetry() };
                let asset = unsafe { board::images::status() };
                let flags = u32::from(sd.ready)
                    | (u32::from(sd.sdhc) << 1)
                    | (u32::from(asset.verified) << 2)
                    | (u32::from(asset.provisioned) << 3)
                    | (u32::from(sd.sdmmc_4bit) << 4)
                    | (u32::from(sd.high_speed) << 5);
                let words = [
                    flags,
                    sd.ocr,
                    sd.sectors,
                    asset.asset_lba,
                    asset.asset_bytes,
                    sd.read_sectors,
                    sd.written_sectors,
                    sd.errors,
                    sd.clk_khz,
                    sd.bench_write_kib_s,
                    sd.bench_read_kib_s,
                ];
                let mut buffer = [0u8; syscall::STATS_SDCARD_EXT_BYTES];
                for (i, word) in words[..len / 4].iter().enumerate() {
                    buffer[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
                }
                copy_to_user(self.memory(), ptr, &buffer[..len])
            }
            syscall::STATS_RANDOM => {
                if len != syscall::STATS_RANDOM_BYTES {
                    return Err(Errno::Einval);
                }
                // SAFETY: serialized by KERNEL_LOCK with IRQs masked.
                let words = unsafe { soc::rng::snapshot() };
                let mut buffer = [0u8; syscall::STATS_RANDOM_BYTES];
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
