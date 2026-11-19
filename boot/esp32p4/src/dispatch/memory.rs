use vela_kernel::{
    capability::{self as cap, Capability, Object},
    errno::Errno,
    id::{Handle, ProcSlot, ThreadSlot},
    process::{READ, WRITE},
    sched::TASKS,
    task::{Context, Region, State, Wait},
};

use super::Args;
use crate::{arch, runtime::Runtime};

impl Runtime {
    pub(super) fn sys_mem_cap(&self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        let region = self
            .memory()
            .maps
            .iter()
            .find(|m| m.region.base == args.a0 as usize)
            .map(|m| m.region)
            .ok_or(Errno::Ebadf)?;
        self.processes
            .slot(current)
            .capabilities
            .find(
                Object::Memory {
                    owner: self.processes.slot(current).id,
                    region,
                },
                cap::CONTROL,
            )
            .map(Handle::raw)
            .ok_or(Errno::Ebadf)
    }

    pub(super) fn sys_mem_alloc(
        &mut self,
        current: ProcSlot,
        args: Args,
        frame: &mut arch::TrapFrame,
    ) -> Result<u32, Errno> {
        let owner = self.processes.slot(current).id;
        let region = self.allocator.allocate(owner, args.a0 as usize)?;
        if let Err(e) = self
            .processes
            .slot_mut(current)
            .memory
            .insert(region, READ | WRITE)
        {
            self.allocator.free(owner, region).unwrap();
            return Err(e);
        }
        if let Err(e) = self
            .processes
            .slot_mut(current)
            .capabilities
            .install(Capability {
                object: Object::Memory { owner, region },
                rights: cap::READ | cap::WRITE | cap::GRANT | cap::CONTROL,
            })
        {
            self.processes
                .slot_mut(current)
                .memory
                .remove(region)
                .unwrap();
            self.allocator.free(owner, region).unwrap();
            return Err(e);
        }
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
        let owner = self.processes.slot(current).id;
        if !self.allocator.owns(owner, region)
            || self
                .processes
                .slot(current)
                .capabilities
                .find(Object::Memory { owner, region }, cap::CONTROL)
                .is_none()
        {
            return Err(Errno::Eperm);
        }
        if self.scheduler.tasks.iter().any(|t| {
            t.process == current
                && t.state != State::Terminated
                && region.contains(t.stack.base, t.stack.size)
        }) {
            return Err(Errno::Ebusy);
        }
        self.invalidate_object(Object::Memory { owner, region }, None);
        self.allocator.free(owner, region).unwrap();
        // Cancel pending receives before the physical region can be reused.
        for i in 0..TASKS {
            let thread = ThreadSlot(i);
            if self.scheduler.task(thread).process != current {
                continue;
            }
            if let Wait::Receive {
                pointer, capacity, ..
            } = self.scheduler.task(thread).wait
            {
                if region.contains(pointer, capacity) {
                    self.scheduler.wake_with(thread, Errno::Efault.to_abi());
                }
            }
        }
        Ok(0)
    }
}
