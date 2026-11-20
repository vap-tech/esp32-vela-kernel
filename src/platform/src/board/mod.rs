pub const NAME: &str = "M5Stack Tab5";
pub mod images;
mod st7121;

use crate::soc::{
    clock,
    display_io::{self as io, Bus},
    dsi,
    psram::{self, Probe as PsramProbe},
    scanout::{Scanout, Snapshot as ScanoutSnapshot},
    sdcard::Probe as SdProbe,
};
use core::fmt;
use vela_kernel::errno::Errno;

const BUS: Bus = Bus { sda: 31, scl: 32 };
const RESET_BITS: u8 = 0x30; // expander 1 P4 LCD, P5 touch
pub const INA226_ADDR: u8 = 0x41; // Tab5 2S battery-rail power monitor (5 mOhm shunt)

/// Initialize the Tab5 HP system I2C bus (`GPIO31` SDA, `GPIO32` SCL).
///
/// # Safety
/// Exclusive HP0 ownership of `GPIO31`/`GPIO32` with machine interrupts masked.
pub unsafe fn init_i2c() {
    unsafe {
        BUS.init();
    }
}

/// Execute a bounded capability-gated I2C transaction on the Tab5 system bus.
/// Only user-accessible board sensors (`0x41` INA226) are permitted; board
/// reset/power I/O expanders (`0x43`, `0x44`) remain kernel-exclusive.
///
/// # Safety
/// Exclusive HP0 ownership of `GPIO31`/`GPIO32` with machine interrupts masked.
pub unsafe fn i2c_transfer(addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), Errno> {
    if !(0x08..=0x77).contains(&addr) || (tx.is_empty() && rx.is_empty()) {
        return Err(Errno::Einval);
    }
    if addr != INA226_ADDR {
        return Err(Errno::Eperm);
    }
    unsafe { BUS.transfer_bytes(addr, tx, rx).map_err(|()| Errno::Eio) }
}

/// Probe the Tab5 microSD card and verify/provision the `VELAIMG1` asset bundle
/// without changing the current MIPI-DSI display or framebuffer state.
///
/// # Safety
/// Exclusive HP0 ownership of microSD SPI (`GPIO39..44`) and Flash staging ROM reads
/// with machine interrupts masked.
pub unsafe fn sdcard_probe() -> (Option<Diagnostic>, Result<u32, u32>) {
    match unsafe { images::probe_and_verify() } {
        Ok(probe) => (Some(Diagnostic::Sdcard(probe)), Ok(0)),
        Err(code) => (None, Err(code)),
    }
}

#[derive(Clone, Copy)]
pub enum Diagnostic {
    Psram(PsramProbe),
    Scanout(ScanoutSnapshot),
    Sdcard(SdProbe),
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Psram(probe) => probe.fmt(f),
            Self::Scanout(snapshot) => snapshot.fmt(f),
            Self::Sdcard(probe) => probe.fmt(f),
        }
    }
}

unsafe fn change(reg: u8, mask: u8, bits: u8) -> Result<(), u32> {
    unsafe {
        let old = BUS.transfer(0x43, &[reg], true).map_err(|_| 1u32)?;
        BUS.transfer(0x43, &[reg, (old & !mask) | bits], false)
            .map_err(|_| 1u32)?;
    }
    Ok(())
}

unsafe fn backlight(on: bool) {
    unsafe {
        io::latch(22, on);
        io::configure(22, false);
        io::drive(22, true);
    }
}

/// Sequential trusted display bring-up steps. Positive result is a minimum delay
/// in 10ms ticks before the next stage; the shell sleeps between stages.
/// When `mode >= 4`, `frame_direct_base` is the direct uncached address of the
/// allocated 2 MiB PSRAM framebuffer (`0x8800_0000 + offset`).
///
/// # Safety
/// Exclusive HP0 ownership of the Tab5 display I2C bus, backlight GPIO/LEDC,
/// CPLL/display clocks, mapped PSRAM framebuffer, MIPI-DSI host/PHY, and DW-GDMA CH0
/// with machine interrupts masked.
pub unsafe fn display_step(
    stage: u8,
    mode: u8,
    frame_direct_base: usize,
    scanout: &mut Scanout,
) -> (Option<Diagnostic>, Result<u32, u32>) {
    let orbit = mode != 1;
    if mode >= 4 && (5..45).contains(&stage) {
        let image = if mode == 4 {
            &images::DIAGNOSTIC
        } else {
            &images::BOOT
        };
        let (sd_probe, res) =
            unsafe { images::draw_rows(image, frame_direct_base, usize::from(stage - 5) * 32, 32) };
        return (sd_probe.map(Diagnostic::Sdcard), res.map(|()| 1));
    }
    let stage = if mode >= 4 && stage >= 45 {
        stage - 40
    } else {
        stage
    };
    unsafe {
        match stage {
            0 => {
                let res = (|| {
                    BUS.init();
                    backlight(false);
                    change(0x05, RESET_BITS, 0)?;
                    change(0x0d, RESET_BITS, RESET_BITS)?;
                    change(0x0b, RESET_BITS, RESET_BITS)?;
                    change(0x07, RESET_BITS, 0)?;
                    change(0x03, RESET_BITS, RESET_BITS)?;
                    Ok(3)
                })();
                (None, res)
            }
            1 => {
                // LCD_RST releases through the expander pull-up, never drives 3.3 V high.
                let res = change(0x03, 0x10, 0)
                    .and_then(|()| change(0x05, 0x20, 0x20))
                    .map(|()| 65);
                (None, res)
            }
            2 => {
                let res = (|| {
                    let version = BUS.transfer(0x55, &[0, 0], true).map_err(|_| 2u32)?;
                    if version != 1 {
                        return Err(0x100 + u32::from(version));
                    }
                    clock::display_speed().map_err(|_| 9u32)?;
                    dsi::power_phy();
                    Ok(2)
                })();
                (None, res)
            }
            3 => {
                let res = (|| {
                    dsi::start().map_err(|_| 3u32)?;
                    for command in st7121::INIT {
                        dsi::dcs(command).map_err(|_| 4u32)?;
                    }
                    dsi::dcs(&[0x11]).map_err(|_| 4u32)?;
                    Ok(13)
                })();
                (None, res)
            }
            4 => (None, dsi::dcs(&[0x29]).map(|()| 81).map_err(|_| 4u32)),
            5 => {
                let res = (|| {
                    for command in [&[0x35, 0][..], &[0x36, 0], &[0x3a, 0x55]] {
                        dsi::dcs(command).map_err(|_| 4u32)?;
                    }
                    if !dsi::video(!orbit) {
                        return Err(5);
                    }
                    if !io::backlight_pwm(22, 160) {
                        return Err(7);
                    }
                    if orbit {
                        let bus_addr = if mode >= 4 {
                            psram::bus_address(frame_direct_base)
                        } else {
                            0
                        };
                        scanout.start(
                            include_bytes!("orbit.a8"),
                            [0x85, 0xbf, 0xff],
                            mode,
                            bus_addr,
                        );
                        return Ok(12);
                    }
                    Ok(0)
                })();
                (None, res)
            }
            6 if orbit => {
                scanout.clear_underrun();
                (None, Ok(50))
            }
            7 if orbit => {
                let snapshot = scanout.snapshot();
                let failed = snapshot.frames < 2 || snapshot.underrun;
                (
                    Some(Diagnostic::Scanout(snapshot)),
                    if failed { Err(8) } else { Ok(0) },
                )
            }
            _ => (None, Err(6)),
        }
    }
}
