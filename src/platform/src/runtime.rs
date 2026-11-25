use core::{cell::UnsafeCell, fmt::Write, mem::MaybeUninit};
use vela_kernel::{
    call::CallTable,
    capability::{self as cap, Capability, Object},
    dma::{DmaDirection, DmaTable},
    errno::Errno,
    event::EventTable,
    id::{Handle, Pid, ProcSlot, ThreadSlot, Ticks, Tid},
    ipc::Ipc,
    lifecycle::{self, ReapedProcess},
    memory::{MemoryClass, RegionAllocator},
    process::{AddressSpace, EXECUTE, Process, Processes, READ, WRITE},
    sched::{HARTS, Scheduler, TASKS},
    stats::{CycleAccounting, FreqMode, MAX_TICKLESS_TICKS, compute_tickless_ticks},
    task::{Region, State, Wait},
};

use crate::{
    arch,
    display::DisplayControl,
    dma::{DMA_CH_GDMA_AHB_CH1, finish_dma_buffer},
    pmp,
    sentinel::SentinelClient,
    soc, syscall,
    usermem::copy_to_user_within,
};

#[derive(Clone, Copy)]
pub struct LoadedElf {
    pub slot: ProcSlot,
    pub pid: Pid,
    pub entry: usize,
    pub stack: Region,
}

pub struct BootLogs {
    pub block: bool,
    pub wake: bool,
    pub idle: bool,
}

impl BootLogs {
    pub const EMPTY: Self = Self {
        block: false,
        wake: false,
        idle: false,
    };
}

/// Per-hart runtime state (`HP0` and `HP1`).
pub struct HartState {
    pub active_space: Option<(ProcSlot, u32)>,
    pub idle_frame: arch::TrapFrame,
    pub in_idle: bool,
    pub cycles: CycleAccounting,
    pub ipi_interrupts: u64,
    pub pmp_shootdowns: u64,
}

pub struct Runtime {
    pub scheduler: Scheduler<arch::TrapFrame>,
    /// Per-thread extended hardware context (`RV32F` FPU + `ESP32-P4 v1.3` `PIE`),
    /// indexed by `ThreadSlot` and 16-byte aligned in internal L2MEM SRAM.
    pub ext_contexts: [arch::ExtendedContext; TASKS],
    pub processes: Processes,
    pub sysd_pid: Option<Pid>,
    pub sd_endpoint: Option<Object>,
    pub ext4_pid: Option<Pid>,
    pub fbcon_pid: Option<Pid>,
    pub kbd_pid: Option<Pid>,
    pub harts: [HartState; HARTS],
    /// Per-process prepared PMP CSR image `(revision, prepared, verified_hart_mask)`.
    pub pmp_images: [Option<(u32, pmp::Prepared, u8)>; vela_kernel::process::PROCESSES],
    pub allocator: RegionAllocator,
    pub console: soc::console::Console,
    pub ipc: Ipc,
    pub calls: CallTable,
    pub events: EventTable,
    pub dma: DmaTable,
    pub sentinel: SentinelClient,
    pub display: DisplayControl,
    pub usb_storage: soc::usb_storage::UsbStorage,
    pub sd_export_buffer: Option<Region>,
    pub sd_export_owner: Option<Pid>,
    pub sd_export_interrupted: bool,
    pub sd_epoch: u32,
    pub sd_partition: Option<(u32, u32)>,
    pub dma_interrupts: u64,
    pub sentinel_interrupts: u64,
    pub freq_mode: FreqMode,
    pub active_high_180mhz: bool,
    pub tickless_armed: bool,
    pub tickless_enter_16m: u64,
    pub tickless_period_16m: u64,
    pub logs: BootLogs,
}

pub mod device_wait {
    pub const DMA: u8 = 1;
    pub const SENTINEL_REQ: u8 = 2;
    pub const SENTINEL_WAKE: u8 = 3;
}

pub const IRQ_SLOT_CONSOLE_RX: usize = syscall::IRQ_SLOT_CONSOLE_RX as usize;
pub const IRQ_SLOT_KBD: usize = syscall::IRQ_SLOT_KBD as usize;

pub struct BootCell(pub UnsafeCell<MaybeUninit<Runtime>>);
// SAFETY: Shared access across HP0 and HP1 is serialized by `soc::smp::KERNEL_LOCK`
// with `MIE = 0` throughout every Rust kernel trap path. The M-mode WFI loop has
// no Rust references and never accesses this cell.
unsafe impl Sync for BootCell {}
pub static RUNTIME: BootCell = BootCell(UnsafeCell::new(MaybeUninit::uninit()));

impl Runtime {
    pub fn endpoint(&self, process: ProcSlot, handle: Handle, rights: u16) -> Result<usize, Errno> {
        lifecycle::resolve_endpoint(&self.processes, &self.ipc, process, handle, rights)
    }

    pub fn mask_irq_slot(&mut self, index: usize) {
        if index == IRQ_SLOT_CONSOLE_RX {
            self.console.mask_rx_irq();
        } else if index == IRQ_SLOT_KBD {
            // SAFETY: serialized by KERNEL_LOCK with IRQs masked.
            unsafe { soc::gpio_irq::mask() };
        }
    }

    /// Wake the remote hart via `FROM_CPU` IPI if it is idle with a runnable
    /// compatible thread or if a higher-priority runnable thread can preempt it.
    pub fn notify_remote_hart(&mut self, timer_tick: bool) {
        if !soc::smp::hp1_online() {
            return;
        }
        let remote = 1 - self.scheduler.active_hart.min(HARTS - 1);
        if self.scheduler.needs_reschedule_on(remote, timer_tick) {
            soc::smp::send_ipi(remote);
        }
    }

    pub fn shootdown_stale_remote_pmp(&mut self) {
        Self::sync_remote_pmp(&self.processes, &mut self.harts, self.scheduler.active_hart);
    }

    /// Called with KERNEL_LOCK held after changing permissions, before recycling
    /// memory or handing it to a device. The peer acknowledges entry to M-mode
    /// before taking the lock, then reloads PMP before returning to U-mode.
    pub fn sync_remote_pmp(processes: &Processes, harts: &mut [HartState; HARTS], local: usize) {
        if !soc::smp::hp1_online() {
            return;
        }
        let remote = 1 - local;
        let Some((proc_slot, active_rev)) = harts[remote].active_space else {
            return;
        };
        let proc = processes.slot(proc_slot);
        if proc.state == vela_kernel::process::Lifecycle::Alive
            && proc.memory.revision == active_rev
        {
            return;
        }
        if !soc::smp::is_hart_in_kernel_or_idle(remote) {
            soc::smp::send_ipi(remote);
            let mut spins = 1_000_000u32;
            while !soc::smp::is_hart_in_kernel_or_idle(remote) {
                // Permissions have already changed in software. Keep the lock and
                // stop the kernel if the peer cannot confirm quiescence.
                assert!(spins > 0, "PMP shootdown timeout on hart {remote}");
                spins -= 1;
                core::hint::spin_loop();
            }
        }
        harts[remote].active_space = None;
        harts[remote].pmp_shootdowns = harts[remote].pmp_shootdowns.saturating_add(1);
    }

    /// Switch HP operating point (`40 MHz` XTAL <-> `180 MHz` CPLL) while
    /// integrating `mcycle` capacity on all harts at the exact `SYSTIMER` transition point.
    pub fn apply_cpu_speed(&mut self, high_180mhz: bool) -> Result<u32, Errno> {
        if !high_180mhz && (self.display.is_active() || self.dma.any_active()) {
            return Err(Errno::Ebusy);
        }
        // SAFETY: serialized by KERNEL_LOCK with IRQs masked.
        unsafe {
            let before = soc::clock::rates_mhz();
            if let Some(now_16m) = soc::timer::counter_16mhz() {
                for hart in &mut self.harts {
                    hart.cycles.sync_clock(now_16m, before.cpu);
                }
            }
            let after = soc::clock::set_speed(high_180mhz).map_err(|_| Errno::Eio)?;
            if let Some(now_16m) = soc::timer::counter_16mhz() {
                for hart in &mut self.harts {
                    hart.cycles.sync_clock(now_16m, after.cpu);
                }
            }
            Ok(after.cpu)
        }
    }

    /// Configure the DVFS governor policy (`Auto`, `Pin40Mhz`, `Pin180Mhz`) and
    /// immediately apply the corresponding active operating point.
    pub fn set_freq_mode(&mut self, mode: FreqMode) -> Result<u32, Errno> {
        match mode {
            FreqMode::Pin40Mhz => {
                let cpu = self.apply_cpu_speed(false)?;
                self.freq_mode = FreqMode::Pin40Mhz;
                self.active_high_180mhz = false;
                Ok(cpu)
            }
            FreqMode::Pin180Mhz => {
                let cpu = self.apply_cpu_speed(true)?;
                self.freq_mode = FreqMode::Pin180Mhz;
                self.active_high_180mhz = true;
                Ok(cpu)
            }
            FreqMode::Auto => {
                self.freq_mode = FreqMode::Auto;
                let want_high =
                    self.active_high_180mhz || self.display.is_active() || self.dma.any_active();
                if want_high {
                    self.active_high_180mhz = true;
                    self.apply_cpu_speed(true)
                } else {
                    self.apply_cpu_speed(false)
                }
            }
        }
    }

    /// Re-check pending peripheral events, LP mailbox replies, and IRQ slot
    /// generations under `MIE=0` right before entering `wfi`.
    ///
    /// Returns `true` if any thread was woken for `self.scheduler.active_hart`.
    pub fn poll_pending_before_idle(&mut self) -> bool {
        let hart = self.scheduler.active_hart;
        if hart != 0 {
            return self.scheduler.has_runnable_for(hart);
        }
        let gen_before = self.events.table_generation();
        // 1. If a console waiter is blocked on `IRQ_SLOT_CONSOLE_RX` and the
        //    USB Serial/JTAG FIFO already has bytes ready, deliver the event now.
        if !self.events.is_masked(IRQ_SLOT_CONSOLE_RX) && self.console.rx_available() {
            self.console.mask_rx_irq();
            if self
                .events
                .notify(IRQ_SLOT_CONSOLE_RX, &mut self.scheduler)
                .is_some()
            {
                self.notify_remote_hart(false);
                return self.scheduler.has_runnable_for(0);
            }
        }
        // 1b. Same for the keyboard `INT` pin: it may have gone low while HP0 was not
        //     waiting with the line armed.
        if !self.events.is_masked(IRQ_SLOT_KBD) && unsafe { soc::gpio_irq::asserted() } {
            unsafe { soc::gpio_irq::mask() };
            if self
                .events
                .notify(IRQ_SLOT_KBD, &mut self.scheduler)
                .is_some()
            {
                self.notify_remote_hart(false);
                return self.scheduler.has_runnable_for(0);
            }
        }
        // 2. If the LP Sentinel mailbox doorbell is asserted, consume and dispatch it.
        if unsafe { soc::sentinel::has_pending_irq() } {
            self.sentinel_interrupts += 1;
            if let Some(reply) = unsafe { soc::sentinel::acknowledge(self.sentinel.epoch()) } {
                self.sentinel
                    .handle_reply(reply, &mut self.scheduler, &self.processes);
                self.notify_remote_hart(false);
            }
        }
        // 3. Re-verify if any thread became runnable or if an event/generation changed.
        self.scheduler.has_runnable_for(0)
            || self.events.table_generation() != gen_before
            || (self.events.any_pending() && self.scheduler.has_runnable_for(0))
    }

    /// Program `SYSTIMER` target 0 for the next idle window (`1..=MAX_TICKLESS_TICKS`)
    /// and apply the DVFS idle operating point (`40 MHz` XTAL when both harts are idle).
    pub fn enter_tickless_idle(&mut self) {
        let now = unsafe { soc::timer::uptime_ticks() }
            .map(Ticks)
            .unwrap_or_else(|| self.scheduler.uptime());
        self.sentinel.tick(now);
        let hp1_busy = soc::smp::hp1_online()
            && (!self.harts[1].in_idle || self.scheduler.has_runnable_for(1));
        let sleep_ticks = compute_tickless_ticks(
            now,
            self.scheduler.nearest_deadline(),
            self.sentinel.next_deadline(),
            self.scheduler.next_load_sample_at(),
            self.display.is_active(),
            self.dma.any_active() || hp1_busy,
            MAX_TICKLESS_TICKS,
        );
        // Downshift to 40 MHz XTAL during tickless sleep (`sleep_ticks >= 2` or
        // coordinated `PREPARE_SLEEP`) when in Auto or Pin40Mhz mode, HP1 is also
        // idle, and no high-bandwidth peripheral is active.
        let can_downshift = !self.display.is_active()
            && !self.dma.any_active()
            && !hp1_busy
            && matches!(self.freq_mode, FreqMode::Auto | FreqMode::Pin40Mhz)
            && (sleep_ticks >= 2 || self.sentinel.is_sleep_prepared());
        if can_downshift {
            let _ = self.apply_cpu_speed(false);
        }
        if sleep_ticks > 1 {
            // SAFETY: sole hart0 SYSTIMER owner under masked IRQs.
            if let Some(now_16m) = unsafe { soc::timer::counter_16mhz() } {
                let (period_16m, expected_16m) =
                    unsafe { soc::timer::arm_period_ticks(sleep_ticks) };
                self.tickless_armed = true;
                self.tickless_enter_16m = now_16m;
                self.tickless_period_16m = period_16m;
                self.harts[0].cycles.record_tickless_entry();
                self.harts[0].cycles.set_expected_irq_16m(expected_16m);
            }
        }
    }

    /// Restore periodic 10 ms `SYSTIMER` ticks and the active DVFS operating point
    /// upon waking `HP0` from a tickless idle window.
    pub fn exit_tickless_idle(&mut self, timer_irq: bool) {
        // SAFETY: sole hart0 SYSTIMER owner under masked IRQs.
        let now_16m =
            unsafe { soc::timer::counter_16mhz() }.expect("SYSTIMER tickless exit snapshot");
        if self.tickless_armed {
            let elapsed_ticks =
                now_16m.saturating_sub(self.tickless_enter_16m) / soc::timer::PERIOD_10MS_16M;
            self.harts[0]
                .cycles
                .record_tickless_wake(elapsed_ticks.max(1));
            if timer_irq {
                let live_tick = self.scheduler.snapshot().scheduler_ticks > 0;
                self.harts[0]
                    .cycles
                    .on_timer_irq(now_16m, self.tickless_period_16m, live_tick);
            }
            let next_periodic_16m = unsafe { soc::timer::restore_periodic_10ms() };
            self.harts[0].cycles.set_expected_irq_16m(next_periodic_16m);
            self.tickless_armed = false;
            self.tickless_period_16m = soc::timer::PERIOD_10MS_16M;
            if !timer_irq {
                let uptime = Ticks(now_16m / soc::timer::PERIOD_10MS_16M);
                self.sentinel.tick(uptime);
                self.scheduler.sync_uptime(uptime, &mut self.calls);
            }
        }
        let restore_high = match self.freq_mode {
            FreqMode::Pin180Mhz => true,
            FreqMode::Pin40Mhz => self.display.is_active() || self.dma.any_active(),
            FreqMode::Auto => {
                self.active_high_180mhz || self.display.is_active() || self.dma.any_active()
            }
        };
        if restore_high && self.harts[0].cycles.cpu_mhz() < 180 {
            let _ = self.apply_cpu_speed(true);
        }
    }

    pub fn ensure_psram_pool(&mut self) -> Result<(), Errno> {
        if !self.display.psram_ready() {
            let (probe, res) = self.display.ensure_psram();
            if let Some(probe) = probe {
                let _ = write!(self.console, "{probe}");
            }
            res?;
        }
        if !self.allocator.psram_configured() {
            self.allocator.configure_psram(
                soc::psram::POOL_START,
                soc::psram::POOL_END,
                soc::psram::MMU_PAGE_BYTES,
            );
        }
        Ok(())
    }

    fn release_owner_memory(&mut self, owner: Pid) {
        let display = &mut self.display;
        self.allocator.release_psram_owner_with(owner, |region| {
            let _ = display.set_psram_region_mapped(region, false);
        });
        self.allocator.release_owner(owner);
    }

    pub fn memory(&self) -> &AddressSpace {
        &self.processes.slot(self.scheduler.current().process).memory
    }

    /// Ensure hardware PMP on `hart` matches `process`'s current `AddressSpace` revision.
    ///
    /// # Safety
    /// Must be called on `hart` in M-mode with IRQs masked after `activate_initial`.
    #[inline(always)]
    pub unsafe fn ensure_active_space_on(&mut self, hart: usize, process: ProcSlot) -> bool {
        let revision = self.processes.slot(process).memory.revision;
        if self.harts[hart].active_space == Some((process, revision)) {
            return true;
        }
        let slot = process.index();
        let hart_bit = 1u8 << hart;
        let (image, verified_mask) = match self.pmp_images[slot] {
            Some((rev, image, mask)) if rev == revision => (image, mask),
            _ => {
                let memory = &self.processes.slot(process).memory;
                let Ok(image) = pmp::Prepared::new(memory) else {
                    return false;
                };
                (image, 0)
            }
        };
        let verified_on_hart = (verified_mask & hart_bit) != 0;
        if !unsafe { image.activate(!verified_on_hart) } {
            return false;
        }
        if !verified_on_hart {
            self.pmp_images[slot] = Some((revision, image, verified_mask | hart_bit));
        }
        self.harts[hart].active_space = Some((process, revision));
        true
    }

    pub fn readable(&self, pointer: usize, length: usize) -> bool {
        self.memory().permits(pointer, length, READ)
    }

    pub fn writable(&self, pointer: usize, length: usize) -> bool {
        self.memory().permits(pointer, length, WRITE)
    }

    pub fn note_woken_receiver(&mut self, woken: Option<Tid>) {
        if let Some(tid) = woken
            && !self.logs.wake
        {
            self.logs.wake = true;
            let _ = writeln!(self.console, "[ipc] receiver woke task={}", tid.raw());
        }
    }

    pub fn complete_receivers(&mut self, endpoint: usize, closed: bool) {
        let woken = lifecycle::complete_receivers(
            &self.processes,
            &mut self.ipc,
            &mut self.calls,
            &mut self.scheduler,
            endpoint,
            closed,
            copy_to_user_within,
        );
        self.note_woken_receiver(woken);
    }

    pub fn finish_reaped_process(&mut self, reaped: ReapedProcess) {
        for (index, unbound) in reaped.unbound_events.into_iter().enumerate() {
            if unbound {
                self.mask_irq_slot(index);
            }
        }
        if self.kbd_pid == Some(reaped.pid) {
            self.kbd_pid = None;
        }
        if self.sysd_pid == Some(reaped.pid) {
            self.sysd_pid = None;
        }
        if self.ext4_pid == Some(reaped.pid) {
            self.ext4_pid = None;
            self.sd_endpoint = None;
        }
        if self.fbcon_pid == Some(reaped.pid) {
            self.fbcon_pid = None;
        }
        self.sentinel.release_wake_on_exit(reaped.pid);
        if self.sd_export_owner == Some(reaped.pid) {
            self.stop_sd_export(false);
        }
        self.dma.release_all_for_process(reaped.pid);
        self.pmp_images[reaped.slot.index()] = None;
        let local = self.scheduler.active_hart;
        if self.harts[local]
            .active_space
            .is_some_and(|(slot, _)| slot == reaped.slot)
        {
            self.harts[local].active_space = None;
        }
        self.note_woken_receiver(reaped.first_woken);
    }

    pub fn terminate(&mut self) {
        let local = self.scheduler.active_hart;
        let harts = &mut self.harts;
        let dma = &mut self.dma;
        let display = &mut self.display;
        if let Some(reaped) = lifecycle::terminate_current_thread(
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
        ) {
            self.finish_reaped_process(reaped);
        }
    }

    pub fn load_elf(
        &mut self,
        bytes: &[u8],
        dma_driver: bool,
    ) -> Result<(LoadedElf, u32, u32), Errno> {
        self.load_elf_in(bytes, MemoryClass::Sram, dma_driver)
    }

    pub fn load_elf_in(
        &mut self,
        bytes: &[u8],
        code_class: MemoryClass,
        dma_driver: bool,
    ) -> Result<(LoadedElf, u32, u32), Errno> {
        let image = vela_kernel::elf::Image::parse(bytes).map_err(|_| Errno::Enoexec)?;
        if code_class == MemoryClass::Psram {
            if !image.pic {
                return Err(Errno::Enoexec);
            }
            self.ensure_psram_pool()?;
        }
        let process = self.processes.create(AddressSpace::EMPTY)?;
        let owner = self.processes.slot(process).id;
        let result = (|| {
            let mut space = AddressSpace::EMPTY;
            let (bias, backing_end) = if image.pic {
                let mut end = 0usize;
                for s in image.segments() {
                    let (tiles, count) =
                        vela_kernel::elf::tile_pie_segment(s.region.base, s.region.size)
                            .map_err(|_| Errno::Enoexec)?;
                    let last = tiles[count - 1];
                    let seg_end = last.base.checked_add(last.size).ok_or(Errno::Enoexec)?;
                    if seg_end > end {
                        end = seg_end;
                    }
                }
                if end == 0 {
                    return Err(Errno::Enoexec);
                }
                let backing = self.allocator.allocate_in(code_class, owner, end)?;
                if code_class == MemoryClass::Psram
                    && !self.display.set_psram_region_mapped(backing, true)
                {
                    return Err(Errno::Eio);
                }
                (backing.base, backing.base + backing.size)
            } else {
                (0, usize::MAX)
            };
            for (i, segment) in image.segments().iter().enumerate() {
                let mut rights = READ;
                if segment.flags & 1 != 0 {
                    rights |= EXECUTE;
                }
                if segment.flags & 2 != 0 {
                    rights |= WRITE;
                }
                let data = image.data(i).ok_or(Errno::Enoexec)?;
                if image.pic {
                    let (tiles, count) = vela_kernel::elf::tile_pie_segment(
                        segment.region.base,
                        segment.region.size,
                    )
                    .map_err(|_| Errno::Enoexec)?;
                    for tile in &tiles[..count] {
                        let base = bias + tile.base;
                        if base.checked_add(tile.size).is_none_or(|e| e > backing_end) {
                            return Err(Errno::Enoexec);
                        }
                        let region = Region {
                            base,
                            size: tile.size,
                        };
                        space.insert(region, rights)?;
                        // Reserved private SRAM or mapped PSRAM. Clear BSS and PMP padding.
                        unsafe {
                            core::ptr::write_bytes(region.base as *mut u8, 0, region.size);
                        }
                    }
                    // SAFETY: contiguous tiled regions cover `[bias + vaddr .. bias + vaddr + size)`.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            data.as_ptr(),
                            (bias + segment.region.base) as *mut u8,
                            data.len(),
                        );
                    }
                } else {
                    let region =
                        self.allocator
                            .reserve(owner, segment.region.base, segment.region.size)?;
                    space.insert(region, rights)?;
                    // Reserved private SRAM. Clear BSS and all PMP-visible padding.
                    unsafe {
                        core::ptr::write_bytes(region.base as *mut u8, 0, region.size);
                        core::ptr::copy_nonoverlapping(
                            data.as_ptr(),
                            region.base as *mut u8,
                            data.len(),
                        );
                    }
                }
            }
            image
                .visit_relocations(|target, addend| {
                    // Parser validated the destination RW word and image-relative addend.
                    unsafe {
                        ((bias + target) as *mut u32).write((bias + addend) as u32);
                    }
                })
                .map_err(|_| Errno::Enoexec)?;
            let entry = bias + image.entry;
            let stack = if code_class == MemoryClass::Psram {
                match self.allocator.allocate(owner, 8192) {
                    Ok(region) => region,
                    Err(_) => {
                        let region =
                            self.allocator
                                .allocate_in(MemoryClass::Psram, owner, 65536)?;
                        if !self.display.set_psram_region_mapped(region, true) {
                            return Err(Errno::Eio);
                        }
                        region
                    }
                }
            } else {
                self.allocator.allocate(owner, 4096)?
            };
            space.insert(stack, READ | WRITE)?;
            let (arg0, arg1) = if dma_driver {
                let dma_obj = self.dma.acquire(DMA_CH_GDMA_AHB_CH1, owner)?;
                let dma_handle =
                    self.processes
                        .slot_mut(process)
                        .capabilities
                        .install(Capability {
                            object: dma_obj,
                            rights: cap::READ | cap::WRITE | cap::WAIT | cap::CONTROL,
                        })?;
                let page = soc::dma::page();
                space.insert(page, READ | WRITE)?;
                let _ = self
                    .processes
                    .slot_mut(process)
                    .capabilities
                    .install(Capability {
                        object: Object::Memory {
                            owner,
                            region: page,
                        },
                        rights: cap::READ | cap::WRITE | cap::CONTROL,
                    })?;
                unsafe {
                    core::ptr::write_bytes(page.base as *mut u8, 0, page.size);
                }
                (page.base as u32, dma_handle.raw())
            } else {
                (0, 0)
            };
            let prepared = pmp::Prepared::new(&space)?;
            unsafe {
                core::ptr::write_bytes(stack.base as *mut u8, 0, stack.size);
                for mapping in &space.maps {
                    if mapping.region.size != 0
                        && mapping.permissions & EXECUTE != 0
                        && !soc::invalidate_icache(mapping.region.base, mapping.region.size)
                    {
                        return Err(Errno::Eio);
                    }
                }
                core::arch::asm!("fence.i", options(nostack));
            }
            self.processes.slot_mut(process).memory = space;
            self.pmp_images[process.index()] = Some((space.revision, prepared, 0));
            Ok((
                LoadedElf {
                    slot: process,
                    pid: owner,
                    entry,
                    stack,
                },
                arg0,
                arg1,
            ))
        })();
        if result.is_err() {
            self.dma.release_all_for_process(owner);
            self.release_owner_memory(owner);
            self.pmp_images[process.index()] = None;
            *self.processes.slot_mut(process) = Process::EMPTY;
        }
        result
    }

    pub fn start_sysd(&mut self, child: LoadedElf, request: Handle, sd: u32) -> Result<Tid, Errno> {
        use vela_kernel::capability::Authority;
        let caps = &mut self.processes.slot_mut(child.slot).capabilities;
        let mut install = |authority, rights| {
            caps.install(Capability {
                object: Object::Authority(authority),
                rights,
            })
            .map(|h| h.raw())
        };
        let system = install(
            Authority::I2c(syscall::I2C_BUS_SYSTEM),
            cap::READ | cap::WRITE,
        )?;
        let ext1 = install(
            Authority::I2c(syscall::I2C_BUS_EXT1),
            cap::READ | cap::WRITE,
        )?;
        let display = install(Authority::Display, cap::CONTROL)?;
        let sentinel = install(
            Authority::Sentinel,
            cap::READ | cap::WRITE | cap::WAIT | cap::CONTROL,
        )?;
        self.start_loaded(
            child,
            [request.raw(), sd, system, ext1, display, sentinel],
            0,
            1,
        )
    }

    pub fn start_loaded(
        &mut self,
        loaded: LoadedElf,
        args: [u32; 6],
        priority: u8,
        priority_ceiling: u8,
    ) -> Result<Tid, Errno> {
        self.start_loaded_at(
            loaded,
            loaded.stack.base + loaded.stack.size,
            args,
            priority,
            priority_ceiling,
        )
    }

    pub fn start_loaded_at(
        &mut self,
        loaded: LoadedElf,
        stack_pointer: usize,
        args: [u32; 6],
        priority: u8,
        priority_ceiling: u8,
    ) -> Result<Tid, Errno> {
        if !stack_pointer.is_multiple_of(16)
            || !loaded.stack.contains(stack_pointer, 0)
            || stack_pointer == loaded.stack.base
        {
            return Err(Errno::Einval);
        }
        self.processes.slot_mut(loaded.slot).priority_ceiling = priority_ceiling;
        let frame = arch::TrapFrame::user_args(loaded.entry, stack_pointer, args);
        let tid = self.scheduler.create_thread(
            loaded.slot,
            frame,
            loaded.stack,
            loaded.entry,
            priority,
            3,
        )?;
        if let Some(slot) = self.scheduler.find_by_id(tid) {
            self.ext_contexts[slot.index()] = arch::ExtendedContext::ZERO;
        }
        Ok(tid)
    }

    pub fn abort_loaded(&mut self, loaded: LoadedElf) {
        self.dma.release_all_for_process(loaded.pid);
        self.release_owner_memory(loaded.pid);
        self.pmp_images[loaded.slot.index()] = None;
        *self.processes.slot_mut(loaded.slot) = Process::EMPTY;
    }

    pub fn complete_dma(&mut self, success: bool) {
        let (owner_opt, region_opt) = self.dma.finish(DMA_CH_GDMA_AHB_CH1, success);
        let alive_proc = owner_opt.and_then(|owner| self.processes.find_alive(owner));
        if let Some(region) = region_opt {
            if success {
                finish_dma_buffer(region, DmaDirection::MemToMem, false);
            }
            if let Ok((class, freed)) = self.allocator.finish_in_flight(
                region,
                DMA_CH_GDMA_AHB_CH1 as u8,
                alive_proc.is_some(),
                success,
            ) && freed
                && class == MemoryClass::Psram
            {
                let _ = self.display.set_psram_region_mapped(region, false);
            }
        }
        let Some(process) = alive_proc else {
            return;
        };
        if success {
            let region = region_opt.unwrap_or_else(soc::dma::page);
            self.processes
                .slot_mut(process)
                .memory
                .dma_access(region, true)
                .unwrap();
        }
        for i in 0..TASKS {
            let thread = ThreadSlot(i);
            let t = self.scheduler.task(thread);
            if t.process == process
                && t.state == State::Blocked
                && matches!(
                    t.wait,
                    Wait::Device {
                        domain: device_wait::DMA,
                        token,
                        ..
                    } if token == DMA_CH_GDMA_AHB_CH1 as u32
                )
            {
                let result = if success { 0 } else { Errno::Eio.to_abi() };
                self.scheduler.wake_with(thread, result);
            }
        }
        self.notify_remote_hart(false);
    }
}
