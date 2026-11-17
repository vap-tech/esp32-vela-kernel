// Adapted from esp32-p4; MIT, copyright 2026 vitalii petrenko.
//! USB Serial/JTAG console; inherits the PHY setup from ROM.
use core::{fmt, marker::PhantomData};

const FIFO: usize = crate::pac::USB_FIFO;
const CONFIG: usize = crate::pac::USB_CONFIG;

/// Exclusive console with bounded USB backpressure. No allocator, buffering, or USB stack.
pub struct Console {
    _exclusive: PhantomData<*mut ()>,
}

impl Console {
    /// # Safety
    /// Caller must exclusively own USB Serial/JTAG and keep its clock/PHY enabled.
    pub unsafe fn new() -> Self {
        Self {
            _exclusive: PhantomData,
        }
    }

    fn put(&mut self, byte: u8) -> bool {
        unsafe {
            // SAFETY: Console owns FIFO/config; ROM PHY is preserved.
            // A battery-powered boot must not wait for a USB host.
            let mut budget = 10_000;
            while crate::pac::read(CONFIG) & 2 == 0 {
                if budget == 0 {
                    return false;
                }
                budget -= 1;
                core::hint::spin_loop();
            }
            crate::pac::write(FIFO, byte as u32);
        }
        true
    }

    /// Return the actual number of user bytes submitted, with bounded polling.
    pub fn write_bytes(&mut self, bytes: &[u8]) -> usize {
        let mut written = 0;
        for &byte in bytes {
            if byte == b'\n' && !self.put(b'\r') {
                break;
            }
            if !self.put(byte) {
                break;
            }
            written += 1;
        }
        self.flush();
        written
    }

    fn flush(&mut self) {
        // SAFETY: exclusive config register ownership.
        unsafe { crate::pac::write(CONFIG, 1) };
    }

    /// Read one host byte if available; never blocks.
    pub fn read_byte(&mut self) -> Option<u8> {
        unsafe {
            // SAFETY: Console owns FIFO/config; ROM PHY is preserved.
            if crate::pac::read(CONFIG) & (1 << 2) != 0 {
                Some(crate::pac::read(FIFO) as u8)
            } else {
                None
            }
        }
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.write_bytes(text.as_bytes()) == text.len() {
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }
}
