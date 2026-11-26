//! Capability-based streams. Stderr currently always goes to the console.
use crate::{Error, Result};

#[doc(hidden)]
pub fn init(stdin: u32, stdout: u32) {
    vela_user::io::init(stdin, stdout);
}

pub fn has_stdin() -> bool {
    vela_user::io::stdin() != 0
}

/// Reads at most 96 bytes per call. Zero means EOF or no attached input.
pub fn read(buffer: &mut [u8]) -> Result<usize> {
    vela_user::io::read(buffer).map_err(|_| Error::Io)
}

/// Writes all bytes, yielding on pipe backpressure. Fails when the reader closes.
pub fn write(bytes: &[u8]) -> Result<()> {
    vela_user::io::write(bytes).map_err(|_| Error::Io)
}

pub fn ewrite(bytes: &[u8]) {
    vela_user::io::ewrite(bytes);
}
