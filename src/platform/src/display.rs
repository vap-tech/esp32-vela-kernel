use vela_kernel::{errno::Errno, task::Region};

use crate::{board, soc};

pub struct DisplayControl {
    mode: u8,
    stage: u8,
    psram: soc::psram::Controller,
    scanout: soc::scanout::Scanout,
}

impl DisplayControl {
    /// Construct the single owner of the display bring-up, PSRAM framebuffer, and
    /// DW-GDMA CH0 scanout engine.
    ///
    /// # Safety
    /// Caller must grant exclusive ownership of the display subsystem (MIPI-DSI,
    /// DW-GDMA CH0, MSPI2/3 PSRAM, LEDC0, and display GPIO/I2C) on HP0, and only
    /// invoke methods with machine interrupts masked.
    pub const unsafe fn new() -> Self {
        Self {
            mode: 1,
            stage: u8::MAX,
            psram: soc::psram::Controller::new(),
            scanout: soc::scanout::Scanout::new(),
        }
    }

    pub const fn psram_ready(&self) -> bool {
        self.psram.is_ready()
    }

    /// Ensure MSPI2/3 AP HEX PSRAM is initialized so the 30 MiB user pool
    /// (`0x8820_0000..0x8a00_0000`) can map and unmap 64 KiB MMU pages.
    pub fn ensure_psram(&mut self) -> (Option<soc::psram::Probe>, Result<(), Errno>) {
        // SAFETY: DisplayControl is owned by Runtime on HP0 with MIE=0.
        let (probe, res) = unsafe { self.psram.initialize() };
        (probe, res.map_err(|_| Errno::Eio))
    }

    /// Map or unmap 64 KiB PSRAM MMU pages for `region` in the managed user pool.
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

    pub fn step(&mut self, arg: u32) -> (Option<board::Diagnostic>, Result<u32, Errno>) {
        if matches!(arg, 1..=5) {
            // SAFETY: DisplayControl owns DW-GDMA CH0 and DSI bridge under MIE=0.
            unsafe {
                self.scanout.stop();
            }
            self.mode = arg as u8;
            self.stage = 0;
        } else if arg != 0 {
            return (None, Err(Errno::Einval));
        }
        // SAFETY: DisplayControl owns the display/PSRAM/scanout hardware under MIE=0.
        let (diag, step) = unsafe {
            board::display_step(self.stage, self.mode, &mut self.psram, &mut self.scanout)
        };
        match step {
            Ok(delay) => {
                self.stage = if delay == 0 { u8::MAX } else { self.stage + 1 };
                (diag, Ok(delay))
            }
            Err(code) => {
                self.stage = u8::MAX;
                // Display driver returns board-specific diagnostic codes (1..18, 256..511),
                // encoded as negative integers directly in the DISPLAY_TEST ABI.
                (diag, Ok(0u32.wrapping_sub(code)))
            }
        }
    }
}
