use vela_kernel::{
    capability as cap,
    errno::Errno,
    id::{Handle, Pid, ProcSlot},
    task::Wait,
};

use super::Args;
use crate::{
    board, pmp,
    runtime::{CONSOLE_RX, Runtime},
    soc,
};

impl Runtime {
    pub(super) fn sys_write(&mut self, args: Args) -> Result<u32, Errno> {
        let length = args.a1 as usize;
        if length > 128 {
            return Err(Errno::Einval);
        }
        if !self.readable(args.a0 as usize, length) {
            return Err(Errno::Efault);
        }
        // SAFETY: validated entire readable range, single hart, IRQs masked.
        let bytes = unsafe { core::slice::from_raw_parts(args.a0 as *const u8, length) };
        Ok(self.console.write_bytes(bytes) as u32)
    }

    pub(super) fn sys_display_test(&mut self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        if self.processes.slot(current).id != Pid(2) {
            return Err(Errno::Eperm);
        }
        if matches!(args.a0, 1..=5) {
            soc::scanout::stop();
            self.display_mode = args.a0 as u8;
            self.display_stage = 0;
        } else if args.a0 != 0 {
            return Err(Errno::Einval);
        }
        match board::display_step(self.display_stage, self.display_mode) {
            Ok(delay) => {
                self.display_stage = if delay == 0 {
                    u8::MAX
                } else {
                    self.display_stage + 1
                };
                Ok(delay)
            }
            Err(code) => {
                self.display_stage = u8::MAX;
                // Display driver returns board-specific diagnostic codes (1..9, 256..511),
                // encoded as negative integers directly in the DISPLAY_TEST ABI.
                Ok(0u32.wrapping_sub(code))
            }
        }
    }

    pub(super) fn sys_dma_copy(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let pid = self.processes.slot(current).id;
        if self.dma != vela_kernel::dma::Lease::Cpu(pid) {
            return Err(Errno::Eperm);
        }
        let len = args.a0 as usize;
        if len == 0 || len > soc::dma::HALF || len % 4 != 0 {
            return Err(Errno::Einval);
        }
        let space = &mut self.processes.slot_mut(current).memory;
        space.dma_access(soc::dma::page(), false)?;
        let prepared = pmp::Prepared::new(space).unwrap();
        assert!(unsafe { prepared.activate() });
        self.pmp_images[current.index()] = Some((space.revision, prepared));
        self.active_space = Some((current, space.revision));
        assert!(self.dma.submit(pid));
        self.dma_length = len;
        self.dma_deadline = self.scheduler.uptime().saturating_add(100);
        self.scheduler.block(Wait::Dma);
        unsafe {
            soc::dma::start(len);
        }
        *switch = true;
        Ok(0)
    }

    pub(super) fn sys_event_wait(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let caps = &self.processes.slot(current).capabilities;
        let capability = caps.get(Handle(args.a0), cap::WAIT | cap::ACK)?;
        if capability.object != CONSOLE_RX {
            return Err(Errno::Ebadf);
        }
        if self
            .scheduler
            .tasks
            .iter()
            .any(|t| matches!(t.wait, Wait::Event { object: CONSOLE_RX }))
        {
            return Err(Errno::Ebusy);
        }
        if self.console.arm_rx_irq() {
            self.scheduler.block(Wait::Event { object: CONSOLE_RX });
            *switch = true;
        }
        Ok(0)
    }

    pub(super) fn sys_console_read(&mut self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        // Boot-assigned input authority: only console task owns RX.
        if self
            .processes
            .slot(current)
            .capabilities
            .find(CONSOLE_RX, cap::READ)
            .is_none()
        {
            return Err(Errno::Eperm);
        }
        let capacity = args.a1 as usize;
        if capacity == 0 || capacity > 32 {
            return Err(Errno::Einval);
        }
        if !self.writable(args.a0 as usize, capacity) {
            return Err(Errno::Efault);
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
        Ok(count as u32)
    }
}
