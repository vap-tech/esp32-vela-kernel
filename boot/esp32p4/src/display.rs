use vela_kernel::errno::Errno;

use crate::{board, soc};

pub struct DisplayControl {
    mode: u8,
    stage: u8,
}

impl DisplayControl {
    pub const fn new() -> Self {
        Self {
            mode: 1,
            stage: u8::MAX,
        }
    }

    pub fn step(&mut self, arg: u32) -> Result<u32, Errno> {
        if matches!(arg, 1..=5) {
            soc::scanout::stop();
            self.mode = arg as u8;
            self.stage = 0;
        } else if arg != 0 {
            return Err(Errno::Einval);
        }
        match board::display_step(self.stage, self.mode) {
            Ok(delay) => {
                self.stage = if delay == 0 { u8::MAX } else { self.stage + 1 };
                Ok(delay)
            }
            Err(code) => {
                self.stage = u8::MAX;
                // Display driver returns board-specific diagnostic codes (1..9, 256..511),
                // encoded as negative integers directly in the DISPLAY_TEST ABI.
                Ok(0u32.wrapping_sub(code))
            }
        }
    }
}
