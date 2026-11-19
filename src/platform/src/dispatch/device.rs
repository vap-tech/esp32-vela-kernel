use vela_kernel::{
    capability::{self as cap, Authority},
    errno::Errno,
    id::{Handle, ProcSlot},
    lifecycle,
    task::Wait,
};

use super::Args;
use crate::{
    pmp,
    runtime::{CONSOLE_RX, Runtime, device_wait},
    soc,
    usermem::{copy_from_user, copy_to_user_within},
};

impl Runtime {
    pub(super) fn sys_write(&mut self, args: Args) -> Result<u32, Errno> {
        let length = args.a1 as usize;
        if length > 128 {
            return Err(Errno::Einval);
        }
        let mut buffer = [0u8; 128];
        copy_from_user(self.memory(), args.a0 as usize, &mut buffer[..length])?;
        Ok(self.console.write_bytes(&buffer[..length]) as u32)
    }

    pub(super) fn sys_display_test(&mut self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        lifecycle::require_authority(
            &self.processes,
            current,
            Handle(args.a0),
            Authority::Display,
            cap::CONTROL,
        )?;
        let before_rates = unsafe { soc::clock::rates_mhz() };
        if let Some(now_16m) = unsafe { soc::timer::counter_16mhz() } {
            self.cycles.sync_clock(now_16m, before_rates.cpu);
        }
        let (diag, result) = self.display.step(args.a1);
        let after_rates = unsafe { soc::clock::rates_mhz() };
        if after_rates.cpu != before_rates.cpu {
            if let Some(now_16m) = unsafe { soc::timer::counter_16mhz() } {
                self.cycles.sync_clock(now_16m, after_rates.cpu);
            }
        }
        if let Some(diag) = diag {
            use core::fmt::Write;
            let _ = write!(self.console, "{diag}");
        }
        result
    }

    pub(super) fn sys_dma_copy(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        let pid = self.processes.slot(current).id;
        if !self.dma.is_cpu_owner(pid) {
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
        self.dma
            .submit(pid, len, self.scheduler.uptime().saturating_add(100));
        self.scheduler.block(Wait::Device {
            domain: device_wait::DMA,
            token: 0,
            pointer: 0,
        });
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
        lifecycle::require_event(
            &self.processes,
            current,
            Handle(args.a0),
            CONSOLE_RX,
            cap::WAIT | cap::ACK,
        )?;
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
        lifecycle::require_event(
            &self.processes,
            current,
            Handle(args.a0),
            CONSOLE_RX,
            cap::READ,
        )?;
        let pointer = args.a1 as usize;
        let capacity = args.a2 as usize;
        if capacity == 0 || capacity > 32 {
            return Err(Errno::Einval);
        }
        if !self.writable(pointer, capacity) {
            return Err(Errno::Efault);
        }
        let mut buffer = [0u8; 32];
        let mut count = 0;
        while count < capacity {
            let Some(byte) = self.console.read_byte() else {
                break;
            };
            buffer[count] = byte;
            count += 1;
        }
        copy_to_user_within(self.memory(), pointer, capacity, &buffer[..count])?;
        Ok(count as u32)
    }
}
