use vela_kernel::{
    errno::Errno,
    id::{Pid, ProcSlot, ThreadSlot, Tid},
    process::EXECUTE,
    task::{Region, State, Wait},
};

use super::Args;
use crate::{arch, runtime::Runtime, syscall};

impl Runtime {
    pub(super) fn sys_spawn_hello(&mut self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        if self.processes.slot(current).id != Pid(2) {
            return Err(Errno::Eperm);
        }
        let bytes: &[u8] = match args.a0 {
            0 => include_bytes!(concat!(env!("OUT_DIR"), "/hello.elf")),
            2 => include_bytes!(concat!(env!("OUT_DIR"), "/dma.elf")),
            1 => include_bytes!(concat!(env!("OUT_DIR"), "/pic.elf")),
            _ => return Err(Errno::Einval),
        };
        self.spawn_elf(bytes, args.a0 == 2).map(Pid::raw)
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
        if args.a1 > u32::from(self.processes.slot(current).priority_ceiling) {
            return Err(Errno::Einval);
        }
        let target_tid = Tid(args.a0);
        let thread = self
            .scheduler
            .tasks
            .iter()
            .position(|t| {
                t.id == target_tid && t.process == current && t.state != State::Terminated
            })
            .map(ThreadSlot)
            .ok_or(Errno::Eperm)?;
        self.scheduler.task_mut(thread).priority = args.a1 as u8;
        self.scheduler.inherit();
        *switch = self.scheduler.should_preempt();
        Ok(0)
    }

    pub(super) fn sys_thread_create(
        &mut self,
        current: ProcSlot,
        args: Args,
    ) -> Result<u32, Errno> {
        let entry = args.a0 as usize;
        let stack = Region {
            base: args.a1 as usize,
            size: args.a2 as usize,
        };
        let priority = args.a4;
        let affinity = args.a5;
        let proc = self.processes.slot(current);
        if entry & 1 != 0
            || !self.memory().permits(entry, 2, EXECUTE)
            || stack.base & 15 != 0
            || stack.size < 256
            || stack.size & 15 != 0
            || !self.writable(stack.base, stack.size)
            || priority > u32::from(proc.priority_ceiling)
            || affinity != 1
            || !self.allocator.owns(proc.id, stack)
        {
            return Err(Errno::Einval);
        }
        let end = stack.base + stack.size;
        if self.scheduler.tasks.iter().any(|t| {
            t.process == current
                && t.state != State::Terminated
                && stack.base < t.stack.base + t.stack.size
                && t.stack.base < end
        }) {
            return Err(Errno::Einval);
        }
        let context = arch::TrapFrame::user(entry, end, args.a3);
        self.scheduler
            .create_thread(
                current,
                context,
                stack,
                entry,
                priority as u8,
                affinity as u8,
            )
            .map(Tid::raw)
    }

    pub(super) fn sys_process_exit(
        &mut self,
        current: ProcSlot,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        for thread in &mut self.scheduler.tasks {
            if thread.process == current {
                thread.state = State::Terminated;
                thread.wait = Wait::None;
            }
        }
        self.terminate();
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_ticks(&self) -> Result<u32, Errno> {
        Ok(self.scheduler.uptime().raw() as u32)
    }

    pub(super) fn sys_stats(&self, args: Args) -> Result<u32, Errno> {
        let len = args.a1 as usize;
        if !matches!(len, syscall::STATS_BYTES | syscall::STATS_IRQ_BYTES) {
            return Err(Errno::Einval);
        }
        if !self.writable(args.a0 as usize, len) {
            return Err(Errno::Efault);
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
            self.usb_rx_interrupts as u32,
            (self.usb_rx_interrupts >> 32) as u32,
        ];
        for (i, word) in words[..len / 4].iter().enumerate() {
            for (j, byte) in word.to_le_bytes().iter().enumerate() {
                // SAFETY: checked writable private user stack, byte-aligned ABI.
                unsafe {
                    (args.a0 as *mut u8).add(i * 4 + j).write(*byte);
                }
            }
        }
        Ok(0)
    }

    pub(super) fn sys_sleep(&mut self, args: Args, switch: &mut bool) -> Result<u32, Errno> {
        if args.a0 == 0 || args.a0 > 6000 {
            return Err(Errno::Einval);
        }
        let until = self.scheduler.uptime().saturating_add(u64::from(args.a0));
        self.scheduler.block(Wait::Sleep { until });
        *switch = true;
        Ok(0)
    }
}
