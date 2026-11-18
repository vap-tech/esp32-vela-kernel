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
pub fn display_step(stage: u8) -> Result<u32, u32> {
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
            if !dsi::bars() {
                return Err(5);
            }
            backlight(true);
            Ok(0)
        }
        _ => Err(6),
    }
}
