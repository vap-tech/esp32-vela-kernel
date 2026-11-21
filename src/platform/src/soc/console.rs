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
            // If the 64-byte EP1 TX FIFO is full, kick WR_DONE so the SIE ships the
            // full packet while we poll for space during multi-chunk `write!` formatting.
            let mut kicked = false;
            let mut budget = 40_000;
            while crate::pac::read(CONFIG) & 2 == 0 {
                if !kicked {
                    kicked = true;
                    crate::pac::write(CONFIG, 1);
                }
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
        // SAFETY: exclusive config register ownership; serialize FIFO writes before WR_DONE.
        unsafe {
            core::arch::asm!("fence iorw, iorw", options(nostack));
            crate::pac::write(CONFIG, 1);
            core::arch::asm!("fence iorw, iorw", options(nostack));
        }
    }

    /// Return `true` if the USB Serial/JTAG RX FIFO currently holds at least one host byte.
    pub fn rx_available(&self) -> bool {
        unsafe { crate::pac::read(CONFIG) & (1 << 2) != 0 }
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

// P4 v1.3 TRM chapters 12 and 49. Source 22 routes to HP0 CLIC line 18.
// SERIAL_OUT_RECV_PKT is latched per packet. DATA_AVAIL persists per byte.
pub const RX_LINE: u32 = 18;
const RX_PACKET: u32 = 1 << 2;
impl Console {
    pub fn route_rx_irq(&mut self) {
        unsafe {
            crate::pac::write(FIFO + 0x10, 0);
            crate::pac::write(FIFO + 0x14, RX_PACKET);
            crate::pac::write(0x500d_6058, RX_LINE);
            crate::pac::write(0x2080_1048, 0x3fc00100);
            core::arch::asm!("fence iorw, iorw", options(nostack));
        }
    }
    pub fn mask_rx_irq(&mut self) {
        unsafe {
            crate::pac::write(FIFO + 0x10, 0);
            crate::pac::write(FIFO + 0x14, RX_PACKET);
            core::arch::asm!("fence iorw, iorw", options(nostack));
        }
    }
    /// Called under masked hart interrupts. Clear old notifications, enable the
    /// source, then sample FIFO state to cover arrivals before/during rearm.
    pub fn arm_rx_irq(&mut self) -> bool {
        unsafe {
            crate::pac::write(FIFO + 0x14, RX_PACKET);
            crate::pac::write(FIFO + 0x10, RX_PACKET);
            core::arch::asm!("fence iorw, iorw", options(nostack));
            if crate::pac::read(CONFIG) & 4 != 0 {
                crate::pac::write(FIFO + 0x10, 0);
                crate::pac::write(FIFO + 0x14, RX_PACKET);
                return false;
            }
            true
        }
    }
}
