use vela_kernel::{
    errno::Errno,
    id::{Handle, ProcSlot},
    lifecycle,
    memory::MemoryClass,
    task::{Context, Region},
};

use super::Args;
use crate::{arch, runtime::Runtime};

impl Runtime {
    pub(super) fn sys_mem_cap(&self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        lifecycle::memory_capability(&self.processes, current, args.a0 as usize).map(Handle::raw)
    }

    pub(super) fn sys_mem_alloc(
        &mut self,
        current: ProcSlot,
        args: Args,
        frame: &mut arch::TrapFrame,
    ) -> Result<u32, Errno> {
        frame.set_return2(0);
        let class = MemoryClass::from_abi(args.a1).ok_or(Errno::Einval)?;
        if class == MemoryClass::Psram {
            self.ensure_psram_pool()?;
        }
        let region = lifecycle::alloc_memory_in(
            &mut self.processes,
            &mut self.allocator,
            current,
            class,
            args.a0 as usize,
        )?;
        if class == MemoryClass::Psram && !self.display.set_psram_region_mapped(region, true) {
            let _ = lifecycle::free_memory(
                &mut self.processes,
                &mut self.allocator,
                &mut self.scheduler,
                current,
                region,
                |_| {},
            );
            return Err(Errno::Eio);
        }
        // SAFETY: newly reserved physical SRAM or mapped PSRAM, no aliases or user execution.
        unsafe {
            core::ptr::write_bytes(region.base as *mut u8, 0, region.size);
        }
        frame.set_return2(region.size as u32);
        Ok(region.base as u32)
    }

    pub(super) fn sys_mem_free(&mut self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        let region = Region {
            base: args.a0 as usize,
            size: args.a1 as usize,
        };
        let local = self.scheduler.active_hart;
        let harts = &mut self.harts;
        let class = lifecycle::free_memory(
            &mut self.processes,
            &mut self.allocator,
            &mut self.scheduler,
            current,
            region,
            |processes| Self::sync_remote_pmp(processes, harts, local),
        )?;
        if class == MemoryClass::Psram {
            let _ = self.display.set_psram_region_mapped(region, false);
        }
        Ok(0)
    }
}
