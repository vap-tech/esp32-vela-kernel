use vela_kernel::{
    capability::{self as cap, Capability, Object},
    ipc,
    process::{READ, WRITE},
    sched::TASKS,
    task::{Region, State, Wait},
};

use super::Args;
use crate::{arch, runtime::Runtime, syscall};

impl Runtime {
    pub(super) fn sys_mem_cap(&self, current: usize, args: Args) -> u32 {
        let region = self
            .memory()
            .maps
            .iter()
            .find(|m| m.region.base == args.a0 as usize)
            .map(|m| m.region);
        match region {
            Some(region) => self.processes.slots[current]
                .capabilities
                .find(
                    Object::Memory {
                        owner: self.processes.slots[current].id,
                        region,
                    },
                    cap::CONTROL,
                )
                .unwrap_or(cap::EBADF),
            None => cap::EBADF,
        }
    }

    pub(super) fn sys_mem_alloc(
        &mut self,
        current: usize,
        args: Args,
        frame: &mut arch::TrapFrame,
    ) -> u32 {
        let owner = self.processes.slots[current].id;
        let region = match self.allocator.allocate(owner, args.a0 as usize) {
            Ok(r) => r,
            Err(e) => return (-(e as i32)) as u32,
        };
        if let Err(e) = self.processes.slots[current]
            .memory
            .insert(region, READ | WRITE)
        {
            self.allocator.free(owner, region).unwrap();
            return (-(e as i32)) as u32;
        }
        if let Err(e) = self.processes.slots[current]
            .capabilities
            .install(Capability {
                object: Object::Memory { owner, region },
                rights: cap::READ | cap::WRITE | cap::GRANT | cap::CONTROL,
            })
        {
            self.processes.slots[current].memory.remove(region).unwrap();
            self.allocator.free(owner, region).unwrap();
            return e;
        }
        // SAFETY: newly reserved physical SRAM, no aliases or user execution.
        unsafe {
            core::ptr::write_bytes(region.base as *mut u8, 0, region.size);
        }
        frame.registers[9] = region.size as u32;
        region.base as u32
    }

    pub(super) fn sys_mem_free(&mut self, current: usize, args: Args) -> u32 {
        let region = Region {
            base: args.a0 as usize,
            size: args.a1 as usize,
        };
        let owner = self.processes.slots[current].id;
        if !self.allocator.owns(owner, region)
            || self.processes.slots[current]
                .capabilities
                .find(Object::Memory { owner, region }, cap::CONTROL)
                .is_none()
        {
            return ipc::EPERM;
        }
        if self.scheduler.tasks.iter().any(|t| {
            t.process == current
                && t.state != State::Terminated
                && region.contains(t.stack.base, t.stack.size)
        }) {
            return (-16i32) as u32;
        }
        self.invalidate_object(Object::Memory { owner, region }, None);
        self.allocator.free(owner, region).unwrap();
        // Cancel pending receives before the physical region can be reused.
        for i in 0..TASKS {
            if self.scheduler.tasks[i].process != current {
                continue;
            }
            if let Wait::Receive {
                pointer, capacity, ..
            } = self.scheduler.tasks[i].wait
            {
                if region.contains(pointer, capacity) {
                    self.scheduler.tasks[i].context.registers[8] = syscall::EFAULT;
                    self.scheduler.wake(i);
                }
            }
        }
        0
    }
}
