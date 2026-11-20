//! GPIO matrix and open-drain software I2C for the board bring-up and system sensors.
//! Called only from the serialized board driver on HP0.
use crate::pac::{read, write};
const GPIO: usize = 0x500e0000;

/// # Safety
/// Exclusive ownership of `pin` in IO_MUX and GPIO matrix on HP0.
pub unsafe fn configure(pin: usize, pull_up: bool) {
    unsafe {
        let mux = 0x500e1004 + 4 * pin;
        write(
            mux,
            (read(mux) & !0x7380) | (1 << 12) | (1 << 9) | (u32::from(pull_up) << 8),
        );
        write(GPIO + 0x558 + 4 * pin, 256 | (1 << 10));
    }
}

/// # Safety
/// Exclusive ownership of `pin` output enable register.
pub unsafe fn drive(pin: usize, enable: bool) {
    let offset = if enable { 0x24 } else { 0x28 };
    unsafe { write(GPIO + offset + 12 * (pin / 32), 1 << (pin % 32)) }
}

/// # Safety
/// Exclusive ownership of `pin` output latch register.
pub unsafe fn latch(pin: usize, high: bool) {
    let offset = if high { 8 } else { 12 };
    unsafe { write(GPIO + offset + 12 * (pin / 32), 1 << (pin % 32)) }
}

unsafe fn level(pin: usize) -> bool {
    unsafe { read(GPIO + 0x3c + 4 * (pin / 32)) & (1 << (pin % 32)) != 0 }
}

unsafe fn pause() {
    // HP ROM ets_delay_us, P4 rev1.
    let delay: extern "C" fn(u32) = unsafe { core::mem::transmute(0x4fc0003cusize) };
    delay(5);
}

pub struct Bus {
    pub sda: usize,
    pub scl: usize,
}

impl Bus {
    /// # Safety
    /// Exclusive ownership of `self.sda` and `self.scl` pins on HP0 with IRQs masked.
    pub unsafe fn init(&self) {
        for pin in [self.sda, self.scl] {
            unsafe {
                drive(pin, false);
                latch(pin, false);
                configure(pin, true);
            }
        }
    }

    unsafe fn clock(&self) -> Result<(), ()> {
        unsafe {
            drive(self.scl, false);
            for _ in 0..100 {
                pause();
                if level(self.scl) {
                    return Ok(());
                }
            }
            Err(())
        }
    }

    unsafe fn start(&self) -> Result<(), ()> {
        unsafe {
            drive(self.sda, false);
            pause();
            self.clock()?;
            if !level(self.sda) {
                return Err(());
            }
            drive(self.sda, true);
            pause();
            drive(self.scl, true);
            pause();
            Ok(())
        }
    }

    unsafe fn bit(&self, high: bool) -> Result<bool, ()> {
        unsafe {
            drive(self.sda, !high);
            pause();
            self.clock()?;
            let value = level(self.sda);
            drive(self.scl, true);
            pause();
            Ok(value)
        }
    }

    unsafe fn send(&self, b: u8) -> Result<(), ()> {
        for shift in (0..8).rev() {
            unsafe { self.bit(b & (1 << shift) != 0)? };
        }
        if unsafe { self.bit(true)? } {
            Err(())
        } else {
            Ok(())
        }
    }

    /// Execute a multi-byte write (`tx`), optional repeated-start read (`rx`),
    /// and STOP condition on the open-drain bus. Sends master ACK (`SDA = 0`)
    /// after each received byte except the final byte (`NACK`, `SDA = 1`).
    ///
    /// # Safety
    /// Exclusive ownership of `self.sda` and `self.scl` pins on HP0 with IRQs masked.
    pub unsafe fn transfer_bytes(&self, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), ()> {
        if tx.is_empty() && rx.is_empty() {
            return Err(());
        }
        unsafe {
            let result = (|| {
                if !tx.is_empty() {
                    self.start()?;
                    self.send(addr << 1)?;
                    for &byte in tx {
                        self.send(byte)?;
                    }
                }
                if !rx.is_empty() {
                    self.start()?;
                    self.send((addr << 1) | 1)?;
                    let last = rx.len() - 1;
                    for (i, slot) in rx.iter_mut().enumerate() {
                        let mut data = 0u8;
                        for _ in 0..8 {
                            data = (data << 1) | u8::from(self.bit(true)?);
                        }
                        *slot = data;
                        // Master sends ACK (0) if more bytes follow, NACK (1) on last byte.
                        let _ = self.bit(i == last)?;
                    }
                }
                Ok(())
            })();
            drive(self.sda, true);
            pause();
            let stop = self.clock();
            drive(self.sda, false);
            pause();
            stop.and(result)
        }
    }

    /// # Safety
    /// Exclusive ownership of `self.sda` and `self.scl` pins on HP0 with IRQs masked.
    pub unsafe fn transfer(&self, addr: u8, bytes: &[u8], read_one: bool) -> Result<u8, ()> {
        let mut out = [0u8; 1];
        let rx: &mut [u8] = if read_one { &mut out } else { &mut [] };
        unsafe { self.transfer_bytes(addr, bytes, rx) }.map(|()| out[0])
    }
}

/// LEDC timer 0, channel 0. XTAL / 4 / 256 gives 39.06 kHz without CPU service.
///
/// # Safety
/// Exclusive ownership of LEDC timer 0 / channel 0 and `pin` GPIO routing on HP0.
pub unsafe fn backlight_pwm(pin: usize, duty: u8) -> bool {
    const CLK: usize = 0x500e6000;
    const LEDC: usize = 0x500d3000;
    unsafe {
        write(CLK + 0x20, read(CLK + 0x20) | 1);
        write(CLK + 0xc4, read(CLK + 0xc4) & !(1 << 29));
        write(CLK + 0x9c, (read(CLK + 0x9c) & !7) | 4);
        write(LEDC + 0xa0, 8 | ((4 * 256) << 5) | (1 << 26));
        write(LEDC + 4, 0);
        write(LEDC + 8, u32::from(duty) << 4);
        write(LEDC + 0xc, 1 << 31);
        write(LEDC, (1 << 4) | (1 << 2));
        for _ in 0..10000 {
            if read(LEDC + 0x10) >> 4 == u32::from(duty) {
                write(GPIO + 0x558 + 4 * pin, 126 | (1 << 10));
                return true;
            }
        }
    }
    false
}
