use vela_kernel::{capability as cap, ipc, task::Wait};

use super::Args;
use crate::{
    board, pmp,
    runtime::{CONSOLE_RX, Runtime},
    soc, syscall,
};

impl Runtime {
    pub(super) fn sys_write(&mut self, args: Args) -> u32 {
        let length = args.a1 as usize;
        if length > 128 {
            return syscall::EINVAL;
        }
        if !self.readable(args.a0 as usize, length) {
            return syscall::EFAULT;
        }
        // SAFETY: validated entire readable range, single hart, IRQs masked.
        let bytes = unsafe { core::slice::from_raw_parts(args.a0 as *const u8, length) };
        self.console.write_bytes(bytes) as u32
    }

    pub(super) fn sys_display_test(&mut self, current: usize, args: Args) -> u32 {
        if self.processes.slots[current].id != 2 {
            return cap::EPERM;
        }
        if matches!(args.a0, 1..=5) {
            soc::scanout::stop();
            self.display_mode = args.a0 as u8;
            self.display_stage = 0;
        } else if args.a0 != 0 {
            return syscall::EINVAL;
        }
        match board::display_step(self.display_stage, self.display_mode) {
            Ok(delay) => {
                self.display_stage = if delay == 0 {
                    u8::MAX
                } else {
                    self.display_stage + 1
                };
                delay
            }
            Err(code) => {
                self.display_stage = u8::MAX;
                0u32.wrapping_sub(code)
            }
        }
    }

    pub(super) fn sys_dma_copy(&mut self, current: usize, args: Args, switch: &mut bool) -> u32 {
        let pid = self.processes.slots[current].id;
        if self.dma != vela_kernel::dma::Lease::Cpu(pid) {
            return cap::EPERM;
        }
        let len = args.a0 as usize;
        if len == 0 || len > soc::dma::HALF || len % 4 != 0 {
            return syscall::EINVAL;
        }
        let space = &mut self.processes.slots[current].memory;
        if let Err(e) = space.dma_access(soc::dma::page(), false) {
            return (-(e as i32)) as u32;
        }
        let prepared = pmp::Prepared::new(space).unwrap();
        assert!(unsafe { prepared.activate() });
        self.pmp_images[current] = Some((space.revision, prepared));
        self.active_space = Some((current, space.revision));
        assert!(self.dma.submit(pid));
        self.dma_length = len;
        self.dma_deadline = self.scheduler.snapshot().uptime_ticks.saturating_add(100);
        self.scheduler.block(Wait::Dma);
        unsafe {
            soc::dma::start(len);
        }
        *switch = true;
        0
    }

    pub(super) fn sys_event_wait(&mut self, current: usize, args: Args, switch: &mut bool) -> u32 {
        let caps = &self.processes.slots[current].capabilities;
        match caps.get(args.a0, cap::WAIT | cap::ACK) {
            Ok(c) if c.object == CONSOLE_RX => (),
            Ok(_) => return cap::EBADF,
            Err(e) => return e,
        }
        if self
            .scheduler
            .tasks
            .iter()
            .any(|t| matches!(t.wait, Wait::Event { object: CONSOLE_RX }))
        {
            return (-16i32) as u32;
        }
        if self.console.arm_rx_irq() {
            self.scheduler.block(Wait::Event { object: CONSOLE_RX });
            *switch = true;
        }
        0
    }

    pub(super) fn sys_console_read(&mut self, current: usize, args: Args) -> u32 {
        // Boot-assigned input authority: only console task owns RX.
        if self.processes.slots[current]
            .capabilities
            .find(CONSOLE_RX, cap::READ)
            .is_none()
        {
            return ipc::EPERM;
        }
        let capacity = args.a1 as usize;
        if capacity == 0 || capacity > 32 {
            return syscall::EINVAL;
        }
        if !self.writable(args.a0 as usize, capacity) {
            return syscall::EFAULT;
        }
        let mut count = 0;
        while count < capacity {
            let Some(byte) = self.console.read_byte() else {
                break;
            };
            // SAFETY: checked writable console task buffer.
            unsafe {
                (args.a0 as *mut u8).add(count).write(byte);
            }
            count += 1;
        }
        count as u32
    }
}
