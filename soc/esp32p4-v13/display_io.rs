//! GPIO matrix and open-drain software I2C for the board bring-up probe.
//! Called only from the serialized board driver on HP0.
use crate::pac::{read, write};
const GPIO: usize = 0x500e0000;

pub fn configure(pin: usize, pull_up: bool) {
    unsafe {
        let mux = 0x500e1004 + 4 * pin;
        write(
            mux,
            (read(mux) & !0x7380) | (1 << 12) | (1 << 9) | (u32::from(pull_up) << 8),
        );
        write(GPIO + 0x558 + 4 * pin, 256 | (1 << 10));
    }
}
pub fn drive(pin: usize, enable: bool) {
    let offset = if enable { 0x24 } else { 0x28 };
    unsafe { write(GPIO + offset + 12 * (pin / 32), 1 << (pin % 32)) }
}
pub fn latch(pin: usize, high: bool) {
    let offset = if high { 8 } else { 12 };
    unsafe { write(GPIO + offset + 12 * (pin / 32), 1 << (pin % 32)) }
}
fn level(pin: usize) -> bool {
    unsafe { read(GPIO + 0x3c + 4 * (pin / 32)) & (1 << (pin % 32)) != 0 }
}
fn pause() {
    // HP ROM ets_delay_us, P4 rev1.
    let delay: extern "C" fn(u32) = unsafe { core::mem::transmute(0x4fc0003cusize) };
    delay(5);
}
pub struct Bus {
    pub sda: usize,
    pub scl: usize,
}
impl Bus {
    pub fn init(&self) {
        for pin in [self.sda, self.scl] {
            drive(pin, false);
            latch(pin, false);
            configure(pin, true);
        }
    }
    fn clock(&self) -> Result<(), ()> {
        drive(self.scl, false);
        for _ in 0..100 {
            pause();
            if level(self.scl) {
                return Ok(());
            }
        }
        Err(())
    }
    fn start(&self) -> Result<(), ()> {
        drive(self.sda, false);
        self.clock()?;
        if !level(self.sda) {
            return Err(());
        }
        drive(self.sda, true);
        pause();
        drive(self.scl, true);
        Ok(())
    }
    fn bit(&self, high: bool) -> Result<bool, ()> {
        drive(self.sda, !high);
        pause();
        self.clock()?;
        let value = level(self.sda);
        drive(self.scl, true);
        pause();
        Ok(value)
    }
    fn send(&self, b: u8) -> Result<(), ()> {
        for shift in (0..8).rev() {
            self.bit(b & (1 << shift) != 0)?;
        }
        if self.bit(true)? { Err(()) } else { Ok(()) }
    }
    pub fn transfer(&self, addr: u8, bytes: &[u8], read_one: bool) -> Result<u8, ()> {
        let result = (|| {
            self.start()?;
            self.send(addr << 1)?;
            for &byte in bytes {
                self.send(byte)?;
            }
            let mut data = 0;
            if read_one {
                self.start()?;
                self.send((addr << 1) | 1)?;
                for _ in 0..8 {
                    data = (data << 1) | u8::from(self.bit(true)?);
                }
                self.bit(true)?;
            }
            Ok(data)
        })();
        drive(self.sda, true);
        pause();
        let stop = self.clock();
        drive(self.sda, false);
        pause();
        stop.and(result)
    }
}
