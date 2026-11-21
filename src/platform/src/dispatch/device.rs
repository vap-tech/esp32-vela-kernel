use vela_kernel::{
    capability::{self as cap, Authority},
    dma::DmaDirection,
    errno::Errno,
    event::WaitOutcome,
    id::{Handle, ProcSlot, Ticks},
    lifecycle,
    memory::MemoryClass,
    task::{Context, Wait},
};

use super::Args;
use crate::{
    arch, board,
    dma::{DMA_CH_GDMA_AHB_CH1, prepare_dma_buffer},
    pmp,
    runtime::{IRQ_SLOT_CONSOLE_RX, Runtime, device_wait},
    soc, syscall,
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
            for hart in &mut self.harts {
                hart.cycles.sync_clock(now_16m, before_rates.cpu);
            }
        }
        let (diag, result) = if args.a1 == syscall::DISPLAY_MODE_SDCARD_BENCH {
            // Boost to 180 MHz CPLL during the SDMMC benchmark so descriptor setup and
            // verification never bottleneck the 4-bit 40 MHz SDMMC IDMAC engine.
            let _ = self.apply_cpu_speed(true);
            let sentinel = &mut self.sentinel;
            let scheduler = &mut self.scheduler;
            let processes = &self.processes;
            let sentinel_irqs = &mut self.sentinel_interrupts;
            // SAFETY: serialized by KERNEL_LOCK with MIE=0 on HP0.
            let (d, res) = unsafe {
                board::sdcard_bench(args.a2, args.a3, || {
                    if soc::sentinel::has_pending_irq() {
                        *sentinel_irqs += 1;
                        if let Some(reply) = soc::sentinel::acknowledge(sentinel.epoch()) {
                            sentinel.handle_reply(reply, scheduler, processes);
                        }
                    }
                    if let Some(now) = soc::timer::uptime_ticks().map(Ticks) {
                        sentinel.tick(now);
                    }
                })
            };
            let mapped = match res {
                Ok(v) => Ok(v),
                Err(code) => Ok(0u32.wrapping_sub(code)),
            };
            (d, mapped)
        } else {
            let caller = self.processes.slot(current).id;
            self.display.step(
                args.a1,
                args.a2 as usize,
                caller,
                &mut self.allocator,
                &mut self.dma,
            )
        };
        let after_rates = unsafe { soc::clock::rates_mhz() };
        if after_rates.cpu != before_rates.cpu {
            if let Some(now_16m) = unsafe { soc::timer::counter_16mhz() } {
                for hart in &mut self.harts {
                    hart.cycles.sync_clock(now_16m, after_rates.cpu);
                }
            }
        }
        if after_rates.cpu >= 180 {
            self.active_high_180mhz = true;
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
        let (channel, len) = if args.a1 != 0 {
            let (idx, _) = lifecycle::resolve_dma_channel(
                &self.processes,
                &self.dma,
                current,
                Handle(args.a0),
                cap::READ | cap::WRITE | cap::WAIT,
            )?;
            (idx, args.a1 as usize)
        } else {
            (DMA_CH_GDMA_AHB_CH1, args.a0 as usize)
        };
        if channel != DMA_CH_GDMA_AHB_CH1 || !self.dma.is_cpu_owner(channel, pid) {
            return Err(Errno::Eperm);
        }
        if len == 0 || len > soc::dma::HALF || len % 4 != 0 {
            return Err(Errno::Einval);
        }
        let page = soc::dma::page();
        let space = &mut self.processes.slot_mut(current).memory;
        space.dma_access(page, false)?;
        let prepared = pmp::Prepared::new(space).unwrap();
        assert!(unsafe { prepared.activate(true) });
        let hart = self.scheduler.active_hart;
        self.pmp_images[current.index()] = Some((space.revision, prepared, 1u8 << hart));
        self.harts[hart].active_space = Some((current, space.revision));
        self.shootdown_stale_remote_pmp();
        let deadline = self.scheduler.uptime().saturating_add(100);
        prepare_dma_buffer(page, DmaDirection::MemToMem, false);
        self.dma
            .submit(
                channel,
                pid,
                MemoryClass::Sram,
                page,
                DmaDirection::MemToMem,
                len,
                Some(deadline),
                false,
            )
            .unwrap();
        self.scheduler.block(Wait::Device {
            domain: device_wait::DMA,
            token: channel as u32,
            pointer: page.base,
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
        let timeout = args.a1;
        if timeout > 6000 {
            return Err(Errno::Einval);
        }
        let (_, object) = lifecycle::resolve_event(
            &self.processes,
            &self.events,
            current,
            Handle(args.a0),
            cap::WAIT | cap::ACK,
        )?;
        let deadline = if timeout == 0 {
            None
        } else {
            Some(self.scheduler.uptime().saturating_add(u64::from(timeout)))
        };
        let console = &mut self.console;
        let outcome = self
            .events
            .wait(object, &mut self.scheduler, deadline, |index| {
                if index == IRQ_SLOT_CONSOLE_RX {
                    console.arm_rx_irq()
                } else {
                    true
                }
            })?;
        if outcome == WaitOutcome::Blocked {
            *switch = true;
        }
        Ok(0)
    }

    pub(super) fn sys_irq_ack(&mut self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        let (_, object) = lifecycle::resolve_event(
            &self.processes,
            &self.events,
            current,
            Handle(args.a0),
            cap::ACK,
        )?;
        let console = &mut self.console;
        self.events.ack(object, |index| {
            if index == IRQ_SLOT_CONSOLE_RX {
                let _ = console.arm_rx_irq();
            }
        })?;
        Ok(0)
    }

    pub(super) fn sys_irq_bind(&mut self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        lifecycle::require_authority(
            &self.processes,
            current,
            Handle(args.a0),
            Authority::IrqControl,
            cap::CONTROL,
        )?;
        let index = usize::try_from(args.a1).map_err(|_| Errno::Einval)?;
        let handle = lifecycle::bind_event(
            &mut self.events,
            &mut self.processes,
            &mut self.scheduler,
            current,
            index,
            cap::READ | cap::WAIT | cap::ACK | cap::GRANT | cap::CONTROL,
        )?;
        self.mask_irq_slot(index);
        Ok(handle.raw())
    }

    pub(super) fn sys_irq_unbind(
        &mut self,
        current: ProcSlot,
        args: Args,
        switch: &mut bool,
    ) -> Result<u32, Errno> {
        if let Some(index) = lifecycle::unbind_event(
            &mut self.processes,
            &mut self.events,
            &mut self.scheduler,
            current,
            Handle(args.a0),
        )? {
            self.mask_irq_slot(index);
        }
        *switch = self.scheduler.should_preempt();
        Ok(0)
    }

    pub(super) fn sys_i2c_transfer(&mut self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        let addr = u8::try_from(args.a1).map_err(|_| Errno::Einval)?;
        let tx_ptr = args.a2 as usize;
        let tx_len = args.a3 as usize;
        let rx_ptr = args.a4 as usize;
        let rx_len = args.a5 as usize;
        if tx_len > syscall::I2C_MAX_BYTES || rx_len > syscall::I2C_MAX_BYTES {
            return Err(Errno::Einval);
        }
        // `tx_len == rx_len == 0` is an address-only probe, which needs READ only.
        let mut required_rights = 0u16;
        if tx_len > 0 {
            required_rights |= cap::WRITE;
        }
        if rx_len > 0 || tx_len == 0 {
            required_rights |= cap::READ;
        }
        let bus =
            lifecycle::require_i2c_bus(&self.processes, current, Handle(args.a0), required_rights)?;
        let mut tx = [0u8; syscall::I2C_MAX_BYTES];
        if tx_len > 0 {
            copy_from_user(self.memory(), tx_ptr, &mut tx[..tx_len])?;
        }
        if rx_len > 0 && !self.writable(rx_ptr, rx_len) {
            return Err(Errno::Efault);
        }
        let mut rx = [0u8; syscall::I2C_MAX_BYTES];
        // SAFETY: sole kernel owner of the Tab5 system I2C pins with IRQs masked.
        unsafe {
            board::i2c_transfer(bus, addr, &tx[..tx_len], &mut rx[..rx_len])?;
        }
        if rx_len > 0 {
            copy_to_user_within(self.memory(), rx_ptr, rx_len, &rx[..rx_len])?;
        }
        Ok(0)
    }

    /// Read console output from byte `a3` onward; returns the count and the next cursor.
    pub(super) fn sys_console_log(
        &mut self,
        current: ProcSlot,
        args: Args,
        frame: &mut arch::TrapFrame,
    ) -> Result<u32, Errno> {
        lifecycle::require_authority(
            &self.processes,
            current,
            Handle(args.a0),
            Authority::ConsoleLog,
            cap::READ,
        )?;
        let pointer = args.a1 as usize;
        let capacity = args.a2 as usize;
        if capacity == 0 || capacity > 128 {
            return Err(Errno::Einval);
        }
        if !self.writable(pointer, capacity) {
            return Err(Errno::Efault);
        }
        let mut buffer = [0u8; 128];
        let (count, next) = soc::console::log_read(args.a3, &mut buffer[..capacity]);
        copy_to_user_within(self.memory(), pointer, capacity, &buffer[..count])?;
        frame.set_return2(next);
        Ok(count as u32)
    }

    pub(super) fn sys_console_read(&mut self, current: ProcSlot, args: Args) -> Result<u32, Errno> {
        let (index, _) = lifecycle::resolve_event(
            &self.processes,
            &self.events,
            current,
            Handle(args.a0),
            cap::READ,
        )?;
        if index != IRQ_SLOT_CONSOLE_RX {
            return Err(Errno::Ebadf);
        }
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
