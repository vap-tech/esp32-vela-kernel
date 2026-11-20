use vela_kernel::{
    dma::{DmaDirection, DmaTable},
    errno::Errno,
    id::Pid,
    memory::{MemoryClass, RegionAllocator},
    task::Region,
};

use crate::{board, dma::DMA_CH_DW_GDMA_SCANOUT, soc};

pub struct DisplayControl {
    mode: u8,
    stage: u8,
    framebuffer: Option<Region>,
    psram: soc::psram::Controller,
    scanout: soc::scanout::Scanout,
}

impl DisplayControl {
    /// Construct the single owner of the display bring-up, dynamically allocated
    /// PSRAM framebuffer, and DW-GDMA CH0 scanout engine.
    ///
    /// # Safety
    /// Caller must grant exclusive ownership of the display subsystem (MIPI-DSI,
    /// DW-GDMA CH0, MSPI2/3 PSRAM, LEDC0, and display GPIO/I2C) on HP0, and only
    /// invoke methods with machine interrupts masked.
    pub const unsafe fn new() -> Self {
        Self {
            mode: 1,
            stage: u8::MAX,
            framebuffer: None,
            psram: soc::psram::Controller::new(),
            scanout: soc::scanout::Scanout::new(),
        }
    }

    pub const fn psram_ready(&self) -> bool {
        self.psram.is_ready()
    }

    /// Ensure MSPI2/3 AP HEX PSRAM is initialized so the full 32 MiB pool
    /// (`0x8800_0000..0x8a00_0000`) can map and unmap 64 KiB MMU pages.
    pub fn ensure_psram(&mut self) -> (Option<soc::psram::Probe>, Result<(), Errno>) {
        // SAFETY: DisplayControl is owned by Runtime on HP0 with MIE=0.
        let (probe, res) = unsafe { self.psram.initialize() };
        (probe, res.map_err(|_| Errno::Eio))
    }

    /// Map or unmap 64 KiB PSRAM MMU pages for `region` in the managed 32 MiB pool.
    pub fn set_psram_region_mapped(&mut self, region: Region, mapped: bool) -> bool {
        // SAFETY: DisplayControl is owned by Runtime on HP0 with MIE=0.
        unsafe { self.psram.set_region_mapped(region, mapped) }
    }

    pub fn tick(&mut self) {
        // SAFETY: DisplayControl owns DW-GDMA CH0 and `_orbit_lli`, called on HP0 with MIE=0.
        unsafe {
            self.scanout.tick();
        }
    }

    pub fn telemetry(&self) -> soc::scanout::StreamTelemetry {
        // SAFETY: DisplayControl owns DSI bridge registers under MIE=0.
        unsafe { self.scanout.telemetry() }
    }

    fn stop_scanout_and_release_dma(
        &mut self,
        allocator: &mut RegionAllocator,
        dma: &mut DmaTable,
    ) {
        // SAFETY: DisplayControl owns DW-GDMA CH0 and DSI bridge under MIE=0.
        unsafe {
            self.scanout.stop();
        }
        if dma.is_active(DMA_CH_DW_GDMA_SCANOUT) {
            let (_, region) = dma.finish(DMA_CH_DW_GDMA_SCANOUT, true);
            if let Some(fb) = region {
                let _ = allocator.finish_in_flight(fb, DMA_CH_DW_GDMA_SCANOUT as u8, true, true);
            }
        }
        dma.release_process(DMA_CH_DW_GDMA_SCANOUT, Pid::KERNEL);
    }

    fn release_framebuffer(&mut self, allocator: &mut RegionAllocator, dma: &mut DmaTable) {
        self.stop_scanout_and_release_dma(allocator, dma);
        if let Some(fb) = self.framebuffer.take() {
            let _ = self.set_psram_region_mapped(fb, false);
            let _ = allocator.free(Pid::KERNEL, fb);
        }
    }

    fn ensure_framebuffer(
        &mut self,
        allocator: &mut RegionAllocator,
    ) -> (Option<board::Diagnostic>, Result<Region, u32>) {
        let mut diag = None;
        if !self.psram.is_ready() {
            // SAFETY: DisplayControl owns MSPI2/3 PSRAM on HP0 with MIE=0.
            let (probe, init) = unsafe { self.psram.initialize() };
            diag = probe.map(board::Diagnostic::Psram);
            if let Err(code) = init {
                return (diag, Err(code));
            }
        }
        if !allocator.psram_configured() {
            allocator.configure_psram(
                soc::psram::POOL_START,
                soc::psram::POOL_END,
                soc::psram::MMU_PAGE_BYTES,
            );
        }
        if let Some(fb) = self.framebuffer {
            return (diag, Ok(fb));
        }
        let Ok(fb) =
            allocator.allocate_in(MemoryClass::Psram, Pid::KERNEL, soc::psram::FRAME_BYTES)
        else {
            return (diag, Err(12));
        };
        if !self.set_psram_region_mapped(fb, true) {
            let _ = allocator.free(Pid::KERNEL, fb);
            return (diag, Err(12));
        }
        self.framebuffer = Some(fb);
        (diag, Ok(fb))
    }

    pub fn step(
        &mut self,
        arg: u32,
        allocator: &mut RegionAllocator,
        dma: &mut DmaTable,
    ) -> (Option<board::Diagnostic>, Result<u32, Errno>) {
        if matches!(arg, 1..=5) {
            self.stop_scanout_and_release_dma(allocator, dma);
            if arg < 4 {
                self.release_framebuffer(allocator, dma);
            }
            self.mode = arg as u8;
            self.stage = 0;
        } else if arg != 0 {
            return (None, Err(Errno::Einval));
        }

        let mut early_diag = None;
        if self.mode >= 4 && self.stage == 2 {
            let (diag, fb_res) = self.ensure_framebuffer(allocator);
            early_diag = diag;
            if let Err(code) = fb_res {
                self.stage = u8::MAX;
                return (early_diag, Ok(0u32.wrapping_sub(code)));
            }
        }
        let fb_base = self.framebuffer.map_or(0, |r| r.base);
        let was_running = self.scanout.is_running();
        // SAFETY: DisplayControl owns the display/PSRAM/scanout hardware under MIE=0.
        let (step_diag, step) =
            unsafe { board::display_step(self.stage, self.mode, fb_base, &mut self.scanout) };
        let diag = early_diag.or(step_diag);
        match step {
            Ok(delay) => {
                if !was_running && self.scanout.is_running() {
                    if let Some(fb) = self.framebuffer
                        && self.mode >= 4
                    {
                        if dma.acquire(DMA_CH_DW_GDMA_SCANOUT, Pid::KERNEL).is_ok() {
                            let _ = dma.submit(
                                DMA_CH_DW_GDMA_SCANOUT,
                                Pid::KERNEL,
                                MemoryClass::Psram,
                                fb,
                                DmaDirection::ToDevice,
                                soc::psram::FRAME_BYTES,
                                None,
                                false,
                            );
                        }
                        let _ = allocator.set_in_flight(
                            Pid::KERNEL,
                            fb,
                            Some(DMA_CH_DW_GDMA_SCANOUT as u8),
                        );
                    }
                }
                self.stage = if delay == 0 { u8::MAX } else { self.stage + 1 };
                (diag, Ok(delay))
            }
            Err(code) => {
                self.stage = u8::MAX;
                self.release_framebuffer(allocator, dma);
                // Display driver returns board-specific diagnostic codes (1..18, 256..511),
                // encoded as negative integers directly in the DISPLAY_TEST ABI.
                (diag, Ok(0u32.wrapping_sub(code)))
            }
        }
    }
}
