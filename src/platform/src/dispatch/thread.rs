use vela_kernel::{
    errno::Errno,
    id::{Pid, ProcSlot, Tid},
    lifecycle,
    task::{Region, Wait},
};

use super::Args;
use crate::{
    arch,
    runtime::Runtime,
    syscall,
    usermem::{copy_to_user, copy_to_user_within},
};

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
        _current: ProcSlot,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let reaped = lifecycle::terminate_process(
            &mut self.processes,
            &mut self.ipc,
            &mut self.calls,
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

    pub(super) fn sys_stats(&self, args: Args) -> Result<u32, Errno> {
        let len = args.a1 as usize;
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
            self.usb_rx_interrupts as u32,
            (self.usb_rx_interrupts >> 32) as u32,
        ];
        let mut buffer = [0u8; syscall::STATS_IRQ_BYTES];
        for (i, word) in words[..len / 4].iter().enumerate() {
            buffer[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        copy_to_user(self.memory(), args.a0 as usize, &buffer[..len])?;
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
