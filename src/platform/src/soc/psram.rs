// MIT copyright 2026 vitalii petrenko. See docs/reference-LICENSE and provenance.md.
//! AP HEX PSRAM controller bring-up, ESP32-P4 rev1.x.
//! Based on ESP-IDF v5.5.2 psram_ctrlr_ll and AP HEX mode-register protocol.
//! Trusted HP0 owns MSPI2/3. MSPI0/1 Flash configuration is untouched.
use crate::pac::{read, write};
use core::fmt;

const CTRL: usize = 0x5008e000;
const USER: usize = 0x5008f000;
const CLK: usize = 0x500e6000;
pub const FRAME: usize = 0x48000000;
pub const DIRECT: usize = 0x88000000;
pub const FRAME_BYTES: usize = 720 * 1280 * 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe {
    pub mr01: u32,
    pub mr23: u32,
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "[psram] mr01={:04x} mr23={:04x}", self.mr01, self.mr23)
    }
}

pub struct Controller {
    ready: bool,
}

impl Controller {
    pub const fn new() -> Self {
        Self { ready: false }
    }

    /// Initialize MSPI2/3 AP HEX PSRAM and map the initial 2 MiB framebuffer window.
    ///
    /// # Safety
    /// Exclusive HP0 ownership of MSPI2/3, LDO VO2, PSRAM IO_MUX, and the direct
    /// uncached PSRAM window (`0x8800_0000..+2 MiB`) with machine interrupts masked.
    pub unsafe fn initialize(&mut self) -> (Option<Probe>, Result<(), u32>) {
        if self.ready {
            return (None, Ok(()));
        }
        let mut probe = None;
        let result = unsafe { init_once(&mut probe) };
        if result.is_ok() {
            self.ready = true;
        }
        (probe, result)
    }
}

unsafe fn get(a: usize) -> u32 {
    unsafe { read(a) }
}
unsafe fn put(a: usize, v: u32) {
    unsafe { write(a, v) }
}
unsafe fn update(a: usize, mask: u32, v: u32) {
    unsafe { put(a, (get(a) & !mask) | v) }
}

#[repr(C)]
struct Command {
    command: u16,
    bits: u16,
    address: *const u32,
    address_bits: u32,
    tx: *const u32,
    tx_bits: u32,
    rx: *mut u32,
    rx_bits: u32,
    dummy: u32,
}

unsafe fn transaction(
    command: u16,
    address: u32,
    dummy: u32,
    tx: Option<u32>,
    rx_bits: u32,
) -> Result<u32, u32> {
    unsafe {
        if get(USER) & (1 << 18) != 0 {
            return Err(15);
        }
        let mut result = 0u32;
        let data = tx.unwrap_or(0);
        let mut conf = Command {
            command,
            bits: 16,
            address: &address,
            address_bits: 32,
            tx: if tx.is_some() {
                &data
            } else {
                core::ptr::null()
            },
            tx_bits: if tx.is_some() { 16 } else { 0 },
            rx: if rx_bits != 0 {
                &mut result
            } else {
                core::ptr::null_mut()
            },
            rx_bits,
            dummy,
        };
        let mode: extern "C" fn(u32, u32) = core::mem::transmute(0x4fc00110usize);
        let config: extern "C" fn(u32, *mut Command) = core::mem::transmute(0x4fc00108usize);
        mode(3, 7);
        config(3, &mut conf);
        update(USER + 0xd4, 2, 0); // fixed dummy for initial ID probe
        update(USER + 0x34, 3, 1); // enable only CS1
        put(USER, 1 << 18);
        for _ in 0..100_000 {
            if get(USER) & (1 << 18) == 0 {
                return Ok(get(USER + 0x58));
            }
        }
        Err(15)
    }
}

unsafe fn bus_div(div: u32) {
    let bits = if div == 1 {
        1 << 31
    } else {
        (div - 1) << 16 | (div / 2 - 1) << 8 | (div - 1)
    };
    unsafe {
        put(CTRL + 0x50, bits);
        put(USER + 0x14, bits);
    }
}

unsafe fn init_once(probe_out: &mut Option<Probe>) -> Result<(), u32> {
    unsafe {
        // VO2 powers the dedicated PSRAM domain at 1.8 V.
        let dref = (get(0x5012d04c) >> 28) & 15;
        let mul = (get(0x5012d050) >> 3) & 7;
        let (dref, mul) = if dref != 0 && mul != 0 {
            (dref, mul)
        } else {
            (6, 5)
        };
        update(
            0x501151d4,
            0xf7800000,
            (dref << 28) | (mul << 23) | (1 << 26),
        );
        update(0x501151d0, (1 << 14) | (7 << 9) | (3 << 7), 3 << 7);
        let delay: extern "C" fn(u32) = core::mem::transmute(0x4fc0003cusize);
        update(0x5011515c, 1 << 24, 1 << 24); // shared MSPI PHY supply
        delay(1000);
        // SPLL / 12 = 40 MHz during mode-register initialization.
        update(CLK + 0x14, 1 << 31, 1 << 31);
        update(CLK + 0xc0, (1 << 23) | (1 << 25), (1 << 23) | (1 << 25));
        update(CLK + 0xc0, (1 << 23) | (1 << 25), 0);
        update(CLK + 0x30, 0xfff000, (2 << 12) | (1 << 14) | (1 << 15));
        for off in (0x1c..=0x68).step_by(4) {
            if off == 0x3c || off == 0x68 {
                update(0x500e1200 + off, 1 | (3 << 15), 1 | (2 << 15));
            } else {
                update(0x500e1200 + off, 3 << 12, 2 << 12);
            }
        }
        put(
            CTRL + 0x1a0,
            1 | 2 | (3 << 2) | (3 << 7) | (2 << 25) | (1 << 31),
        );
        update(CTRL + 0x174, 3 << 18, 3 << 18);
        bus_div(12);
        update(CTRL + 0x190, 1 << 5, 1 << 5);
        update(CTRL + 0x180, 1 << 5, 1 << 5);
        let mr01 = transaction(0x4040, 0, 8, None, 16)?;
        let mr23 = transaction(0x4040, 2, 8, None, 16)?;
        *probe_out = Some(Probe { mr01, mr23 });
        if (mr01 >> 8) & 31 != 0x0d && (mr01 >> 8) & 31 != 0x1a {
            return Err(10);
        }
        if mr23 & 7 != 7 {
            return Err(11);
        } // selected Tab5 has 32 MiB
        let mr4 = transaction(0x4040, 4, 8, None, 16)?;
        transaction(0xc0c0, 0, 0, Some((mr01 & !0x3f) | 0x28), 0)?;
        transaction(0xc0c0, 4, 0, Some((mr4 & !0xe0) | 0x40), 0)?;
        let mr8 = transaction(0x4040, 8, 8, None, 8)?;
        transaction(0xc0c0, 8, 0, Some((mr8 & !0x4f) | 0x4b), 0)?; // x16, linear burst, 2 KiB page
        // Configure the AXI port for 16-line DDR synchronous commands.
        put(CTRL + 0x4c, (15 << 28) | 0x8080);
        put(CTRL + 0x48, 15 << 28);
        put(
            CTRL + 0x40,
            1 | (1 << 3)
                | (1 << 4)
                | (1 << 5)
                | (17 << 6)
                | (31 << 14)
                | (1 << 20)
                | (1 << 21)
                | (7 << 22),
        );
        put(
            CTRL + 0x44,
            (1 << 18) | (1 << 19) | (1 << 20) | (1 << 21) | (1 << 23) | (1 << 26) | (1 << 27),
        );
        update(CTRL + 0xd8, 15, 3); // DDR, variable read dummy, no data swapping
        update(CTRL + 0x0c, (1 << 25) | (1 << 26), (1 << 25) | (1 << 26));
        update(CTRL + 0x3c, (1 << 31) | 1, 1); // AXI requests enabled
        bus_div(6); // SPLL / 6 = 80 MHz DDR AXI access
        // Map the first 2 MiB. Access uses the uncached alias, DMA the bus alias.
        for page in 0..32 {
            put(CTRL + 0x380, page);
            put(CTRL + 0x37c, page | (1 << 11) | (1 << 10));
        }
        core::arch::asm!("fence iorw, iorw", options(nostack));
        // Distinct sparse addresses catch disconnected lines and address aliasing.
        for off in (0..FRAME_BYTES).step_by(4096) {
            put(DIRECT + off, 0x5a6b7c8d ^ (off as u32));
        }
        for off in (0..FRAME_BYTES).step_by(4096) {
            if get(DIRECT + off) != 0x5a6b7c8d ^ (off as u32) {
                return Err(12);
            }
        }
        Ok(())
    }
}
