pub const NAME: &str = "M5Stack Tab5";
mod st7121;
use crate::soc::{
    display_io::{self as io, Bus},
    dsi,
};
const BUS: Bus = Bus { sda: 31, scl: 32 };
const RESET_BITS: u8 = 0x30; // expander 1 P4 LCD, P5 touch
fn change(reg: u8, mask: u8, bits: u8) -> Result<(), u32> {
    let old = BUS.transfer(0x43, &[reg], true).map_err(|_| 1u32)?;
    BUS.transfer(0x43, &[reg, (old & !mask) | bits], false)
        .map_err(|_| 1u32)?;
    Ok(())
}
fn backlight(on: bool) {
    io::latch(22, on);
    io::configure(22, false);
    io::drive(22, true);
}
/// Sequential trusted operations. Positive result is a minimum delay in 10ms
/// ticks before the next stage. The shell sleeps between stages.
pub fn display_step(stage: u8, mode: u8) -> Result<u32, u32> {
    let orbit = mode != 1;
    match stage {
        0 => {
            BUS.init();
            backlight(false);
            change(0x05, RESET_BITS, 0)?;
            change(0x0d, RESET_BITS, RESET_BITS)?;
            change(0x0b, RESET_BITS, RESET_BITS)?;
            change(0x07, RESET_BITS, 0)?;
            change(0x03, RESET_BITS, RESET_BITS)?;
            Ok(3)
        }
        1 => {
            // LCD_RST releases through the expander pull-up, never drives 3.3 V high.
            change(0x03, 0x10, 0)?;
            change(0x05, 0x20, 0x20)?;
            Ok(65)
        }
        2 => {
            let version = BUS.transfer(0x55, &[0, 0], true).map_err(|_| 2u32)?;
            if version != 1 {
                return Err(0x100 + u32::from(version));
            }
            crate::soc::clock::display_speed().map_err(|_| 9u32)?;
            dsi::power_phy();
            Ok(2)
        }
        3 => {
            dsi::start().map_err(|_| 3u32)?;
            for command in st7121::INIT {
                dsi::dcs(command).map_err(|_| 4u32)?;
            }
            dsi::dcs(&[0x11]).map_err(|_| 4u32)?;
            Ok(13)
        }
        4 => {
            dsi::dcs(&[0x29]).map_err(|_| 4u32)?;
            Ok(81)
        }
        5 => {
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
                crate::soc::scanout::start(
                    include_bytes!("orbit.a8"),
                    [0x85, 0xbf, 0xff],
                    mode == 3,
                );
                return Ok(12);
            }
            Ok(0)
        }
        6 if orbit => {
            crate::soc::scanout::clear_underrun();
            Ok(50)
        }
        7 if orbit => {
            crate::soc::scanout::debug();
            use core::fmt::Write;
            let _ = writeln!(
                unsafe { crate::soc::console::Console::new() },
                "[scan] frames={} underrun={}",
                crate::soc::scanout::frames(),
                crate::soc::scanout::underrun()
            );
            if crate::soc::scanout::frames() < 2 || crate::soc::scanout::underrun() {
                return Err(8);
            }
            Ok(0)
        }
        _ => Err(6),
    }
}
