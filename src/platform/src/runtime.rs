use core::{cell::UnsafeCell, fmt::Write, mem::MaybeUninit};
use vela_kernel::{
    call::CallTable,
    errno::Errno,
    event::EventTable,
    id::{Handle, Pid, ProcSlot, ThreadSlot, Tid},
    ipc::Ipc,
    lifecycle::{self, ReapedProcess},
    memory::{MemoryClass, RegionAllocator},
    process::{AddressSpace, EXECUTE, Process, Processes, READ, WRITE},
    sched::{Scheduler, TASKS},
    stats::CycleAccounting,
    task::{Region, State, Wait},
};

use crate::{
    arch, display::DisplayControl, dma::DmaChannel, pmp, sentinel::SentinelClient, soc, syscall,
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

pub struct Runtime {
    pub scheduler: Scheduler<arch::TrapFrame>,
    pub processes: Processes,
    pub active_space: Option<(ProcSlot, u32)>,
    pub pmp_images: [Option<(u32, pmp::Prepared, bool)>; vela_kernel::process::PROCESSES],
    pub allocator: RegionAllocator,
    pub console: soc::console::Console,
    pub ipc: Ipc,
    pub calls: CallTable,
    pub events: EventTable,
    pub idle_frame: arch::TrapFrame,
    pub in_idle: bool,
    pub dma: DmaChannel,
    pub sentinel: SentinelClient,
    pub display: DisplayControl,
    pub dma_interrupts: u64,
    pub sentinel_interrupts: u64,
    pub cycles: CycleAccounting,
    pub logs: BootLogs,
}

pub mod device_wait {
    pub const DMA: u8 = 1;
    pub const SENTINEL_REQ: u8 = 2;
    pub const SENTINEL_WAKE: u8 = 3;
}

pub const IRQ_SLOT_CONSOLE_RX: usize = syscall::IRQ_SLOT_CONSOLE_RX as usize;

pub struct BootCell(pub UnsafeCell<MaybeUninit<Runtime>>);
// SAFETY: hart0 only, HP1 reset, MIE=0 in every Rust kernel path. The M-mode
// WFI loop has no Rust references and never accesses this cell. Initialization
// precedes timer enable; outermost traps are the only later borrowers.
unsafe impl Sync for BootCell {}
pub static RUNTIME: BootCell = BootCell(UnsafeCell::new(MaybeUninit::uninit()));

impl Runtime {
    pub fn endpoint(&self, process: ProcSlot, handle: Handle, rights: u16) -> Result<usize, Errno> {
        lifecycle::resolve_endpoint(&self.processes, &self.ipc, process, handle, rights)
    }

    pub fn mask_irq_slot(&mut self, index: usize) {
        if index == IRQ_SLOT_CONSOLE_RX {
            self.console.mask_rx_irq();
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

    /// Ensure hardware PMP matches `process`'s current `AddressSpace` revision.
    ///
    /// # Safety
    /// Local hart M-mode with IRQs masked after `activate_initial` succeeded at boot.
    #[inline(always)]
    pub unsafe fn ensure_active_space(&mut self, process: ProcSlot) -> bool {
        let revision = self.processes.slot(process).memory.revision;
        if self.active_space == Some((process, revision)) {
            return true;
        }
        let slot = process.index();
        let (image, verified) = match self.pmp_images[slot] {
            Some((rev, image, verified)) if rev == revision => (image, verified),
            _ => {
                let memory = &self.processes.slot(process).memory;
                let Ok(image) = pmp::Prepared::new(memory) else {
                    return false;
                };
                (image, false)
            }
        };
        if !unsafe { image.activate(!verified) } {
            return false;
        }
        if !verified {
            self.pmp_images[slot] = Some((revision, image, true));
        }
        self.active_space = Some((process, revision));
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
        self.sentinel.release_wake_on_exit(reaped.pid);
        self.dma.release_process(reaped.pid);
        self.pmp_images[reaped.slot.index()] = None;
        self.note_woken_receiver(reaped.first_woken);
    }

    pub fn terminate(&mut self) {
        let process = self.scheduler.current().process;
        let owner = self.processes.slot(process).id;
        let is_last = !self.scheduler.tasks.iter().enumerate().any(|(i, t)| {
            ThreadSlot(i) != self.scheduler.current
                && t.process == process
                && t.state != State::Terminated
        });
        if is_last {
            let display = &mut self.display;
            self.allocator.release_psram_owner_with(owner, |region| {
                let _ = display.set_psram_region_mapped(region, false);
            });
        }
        if let Some(reaped) = lifecycle::terminate_current_thread(
            &mut self.processes,
            &mut self.ipc,
            &mut self.calls,
            &mut self.events,
            &mut self.allocator,
            &mut self.scheduler,
            copy_to_user_within,
        ) {
            self.finish_reaped_process(reaped);
        }
    }

    pub fn load_elf(&mut self, bytes: &[u8], dma_driver: bool) -> Result<(LoadedElf, u32), Errno> {
        self.load_elf_in(bytes, MemoryClass::Sram, dma_driver)
    }

    pub fn load_elf_in(
        &mut self,
        bytes: &[u8],
        code_class: MemoryClass,
        dma_driver: bool,
    ) -> Result<(LoadedElf, u32), Errno> {
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
            let bias = if image.pic {
                let end = image
                    .segments()
                    .iter()
                    .map(|s| s.region.base + s.region.size)
                    .max()
                    .ok_or(Errno::Enoexec)?;
                let backing = self.allocator.allocate_in(code_class, owner, end)?;
                if code_class == MemoryClass::Psram
                    && !self.display.set_psram_region_mapped(backing, true)
                {
                    return Err(Errno::Eio);
                }
                backing.base
            } else {
                0
            };
            for (i, segment) in image.segments().iter().enumerate() {
                let region = if image.pic {
                    let size = segment
                        .region
                        .size
                        .max(128)
                        .checked_next_power_of_two()
                        .ok_or(Errno::Enoexec)?;
                    if segment.region.base % size != 0 {
                        return Err(Errno::Enoexec);
                    }
                    Region {
                        base: bias + segment.region.base,
                        size,
                    }
                } else {
                    self.allocator
                        .reserve(owner, segment.region.base, segment.region.size)?
                };
                let mut rights = READ;
                if segment.flags & 1 != 0 {
                    rights |= EXECUTE;
                }
                if segment.flags & 2 != 0 {
                    rights |= WRITE;
                }
                space.insert(region, rights)?;
                let data = image.data(i).ok_or(Errno::Enoexec)?;
                // Reserved private SRAM or mapped PSRAM. Clear BSS and all PMP-visible padding.
                unsafe {
                    core::ptr::write_bytes(region.base as *mut u8, 0, region.size);
                    core::ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        region.base as *mut u8,
                        data.len(),
                    );
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
            let stack = self.allocator.allocate(owner, 4096)?;
            space.insert(stack, READ | WRITE)?;
            let arg = if dma_driver {
                if !self.dma.acquire(owner) {
                    return Err(Errno::Ebusy);
                }
                let page = soc::dma::page();
                space.insert(page, READ | WRITE)?;
                unsafe {
                    core::ptr::write_bytes(page.base as *mut u8, 0, page.size);
                }
                page.base as u32
            } else {
                0
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
            self.pmp_images[process.index()] = Some((space.revision, prepared, false));
            Ok((
                LoadedElf {
                    slot: process,
                    pid: owner,
                    entry,
                    stack,
                },
                arg,
            ))
        })();
        if result.is_err() {
            self.dma.release_process(owner);
            self.release_owner_memory(owner);
            self.pmp_images[process.index()] = None;
            *self.processes.slot_mut(process) = Process::EMPTY;
        }
        result
    }

    pub fn start_loaded(
        &mut self,
        loaded: LoadedElf,
        args: [u32; 6],
        priority: u8,
        priority_ceiling: u8,
    ) -> Result<Tid, Errno> {
        self.processes.slot_mut(loaded.slot).priority_ceiling = priority_ceiling;
        let frame =
            arch::TrapFrame::user_args(loaded.entry, loaded.stack.base + loaded.stack.size, args);
        self.scheduler
            .create_thread(loaded.slot, frame, loaded.stack, loaded.entry, priority, 1)
    }

    pub fn abort_loaded(&mut self, loaded: LoadedElf) {
        self.dma.release_process(loaded.pid);
        self.release_owner_memory(loaded.pid);
        self.pmp_images[loaded.slot.index()] = None;
        *self.processes.slot_mut(loaded.slot) = Process::EMPTY;
    }

    pub fn spawn_elf_in(
        &mut self,
        bytes: &[u8],
        code_class: MemoryClass,
        dma_driver: bool,
    ) -> Result<Pid, Errno> {
        let (loaded, arg0) = self.load_elf_in(bytes, code_class, dma_driver)?;
        if let Err(e) = self.start_loaded(loaded, [arg0, 0, 0, 0, 0, 0], 1, 1) {
            self.abort_loaded(loaded);
            return Err(e);
        }
        Ok(loaded.pid)
    }

    pub fn complete_dma(&mut self, success: bool) {
        let Some(owner) = self.dma.finish(success) else {
            return;
        };
        let Some(process) = self.processes.find_alive(owner) else {
            return;
        };
        if success {
            self.processes
                .slot_mut(process)
                .memory
                .dma_access(soc::dma::page(), true)
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
                        ..
                    }
                )
            {
                let result = if success { 0 } else { Errno::Eio.to_abi() };
                self.scheduler.wake_with(thread, result);
            }
        }
    }
}
