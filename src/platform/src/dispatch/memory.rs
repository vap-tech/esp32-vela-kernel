use vela_kernel::{
    errno::Errno,
    id::{Handle, ProcSlot},
    lifecycle,
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
        let region = lifecycle::alloc_memory(
            &mut self.processes,
            &mut self.allocator,
            current,
            args.a0 as usize,
        )?;
        // SAFETY: newly reserved physical SRAM, no aliases or user execution.
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
        lifecycle::free_memory(
            &mut self.processes,
            &mut self.allocator,
            &mut self.scheduler,
            current,
            region,
        )?;
        Ok(0)
    }
}
