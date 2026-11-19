//! Fixed display operating point for P4 rev1.x, following ESP-IDF v5.5.2.
//! CPU/MEM/SYS 180 MHz, APB 90 MHz, CPLL 360 MHz. HP0, IRQs masked.
use crate::pac::{read, write};
const CLK: usize = 0x500e6000;
const ANALOG: usize = 0x50124000;
fn get(addr: usize) -> u32 {
    unsafe { read(addr) }
}
fn put(addr: usize, value: u32) {
    unsafe { write(addr, value) }
}
fn update(addr: usize, mask: u32, value: u32) {
    put(addr, (get(addr) & !mask) | value)
}
fn wait(addr: usize, mask: u32, wanted: u32) -> Result<(), ()> {
    for _ in 0..1_000_000 {
        if get(addr) & mask == wanted {
            return Ok(());
        }
    }
    Err(())
}
fn analog_write(reg: u32, value: u32) -> Result<(), ()> {
    wait(ANALOG, 1 << 25, 0)?;
    put(ANALOG, 0x67 | (reg << 8) | (value << 16) | (1 << 24));
    wait(ANALOG, 1 << 25, 0)
}
fn latch() -> Result<(), ()> {
    update(CLK + 4, 1 << 4, 1 << 4);
    wait(CLK + 4, 1 << 4, 0)
}
/// Called by the trusted display service before starting video DMA.
pub fn display_speed() -> Result<(), ()> {
    if get(0x50111040) & 3 == 1 {
        return Ok(());
    }
    if get(0x50111040) & 3 != 0 {
        return Err(());
    }
    // Enable/calibrate the CPU PLL while HP still runs on XTAL.
    put(0x501150cc, (1 << 27) | (1 << 23));
    put(0x501150cc, 1 << 17);
    update(0x50120000, 1 << 27, 1 << 27);
    update(ANALOG + 0x34, 1, 1);
    put(ANALOG + 0x1c, 0);
    put(ANALOG + 0x20, 1 << 11);
    update(CLK + 0xbc, 1 << 3, 0);
    analog_write(2, 0x50)?;
    // ECO1 swaps bit 2/3 of the feedback divider. Rev1.3 uses value 9.
    analog_write(3, 9)?;
    analog_write(6, 0x73)?;
    wait(CLK + 0xbc, 1 << 2, 1 << 2)?;
    let delay: extern "C" fn(u32) = unsafe { core::mem::transmute(0x4fc0003cusize) };
    delay(10);
    update(CLK + 0xbc, 1 << 3, 1 << 3);
    // Upscale dividers from the peripheral side before switching the source.
    update(CLK + 0xc, 0xffff0000, 1 << 16);
    latch()?;
    update(CLK + 8, u32::MAX, 0);
    latch()?;
    update(CLK + 4, 0x1fffffe0, 1 << 5);
    latch()?;
    update(0x50111040, 3, 1);
    let set_ticks: extern "C" fn(u32) = unsafe { core::mem::transmute(0x4fc00044usize) };
    set_ticks(180);
    Ok(())
}
