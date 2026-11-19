//! RGB565 scanout on the dedicated DesignWare AXI DMA channel 0.
//! HP0 owns a four-frame ring. Each frame has a fixed black source, an
//! incrementing image span, and the fixed black source again.
//! Orbit stores a 64-row RGB565 band. Pixel mode uses a single source word.
//! All CPU accesses use the direct SRAM alias.
use crate::pac::{read, write};
use core::sync::atomic::{AtomicU32, Ordering};
unsafe extern "C" {
    static _orbit_lli: u8;
    static _orbit_black: u8;
    static _orbit_band: u8;
}
const DIRECT: usize = 0x40000000;
const DMA: usize = 0x50081000;
const BRIDGE: usize = 0x500a0800;
const WIDTH: usize = 720;
const HEIGHT: usize = 1280;
const ICON: usize = 64;
const FRAME_DONE: u32 = 2;
const ERRORS: u32 = 0x3fe0;
const VALID: u32 = 1 << 31;
const SLOTS: usize = 4;
const PER_FRAME: usize = 3;
const NODES: usize = PER_FRAME * SLOTS;
const BURSTS: u32 = (1 << 6) | (16 << 7) | (1 << 15) | (16 << 16);

static FRAMES: AtomicU32 = AtomicU32::new(0);
static RUNNING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
fn reg(addr: usize) -> u32 {
    unsafe { read(addr) }
}
fn put(addr: usize, val: u32) {
    unsafe { write(addr, val) }
}
fn barrier() {
    unsafe { core::arch::asm!("fence iorw, iorw", options(nostack)) }
}
fn descriptors() -> usize {
    core::ptr::addr_of!(_orbit_lli) as usize
}
pub fn tick() {
    if !RUNNING.load(Ordering::Relaxed) {
        return;
    }
    for i in 0..NODES {
        let ctl = DIRECT + descriptors() + 64 * i + 36;
        if reg(ctl) & VALID == 0 {
            put(ctl, BURSTS | VALID);
            if i % PER_FRAME == PER_FRAME - 1 && FRAMES.fetch_add(1, Ordering::Relaxed) == 0 {
                put(BRIDGE + 0x54, 1);
            }
        }
    }
    barrier();
}
pub fn stop() {
    if RUNNING.swap(false, Ordering::Relaxed) {
        put(BRIDGE + 0x40, 0);
        put(BRIDGE + 0x44, 1);
        // This driver exclusively owns DW-GDMA; AHB DMA is independent.
        put(0x500e60c0, reg(0x500e60c0) | (1 << 21));
        put(0x500e60c0, reg(0x500e60c0) & !(1 << 21));
    }
}
pub fn debug() {
    use core::fmt::Write;
    let mut console = unsafe { crate::soc::console::Console::new() };
    for off in [
        8, 0x10, 0x18, 0x30, 0x50, 0x100, 0x108, 0x110, 0x118, 0x120, 0x124, 0x128, 0x130, 0x180,
        0x188, 0x190,
    ] {
        let _ = write!(console, "{:x}={:x} ", off, reg(DMA + off));
    }
    let _ = writeln!(
        console,
        "fifo={:x} clk={:x}/{:x}/{:x} lp={:x}",
        reg(BRIDGE + 0x14),
        reg(0x500e6004),
        reg(0x500e6008),
        reg(0x500e600c),
        reg(0x50111040)
    );
}
pub fn frames() -> u32 {
    FRAMES.load(Ordering::Relaxed)
}
pub fn clear_underrun() {
    put(BRIDGE + 0x54, 1);
}
pub fn underrun() -> bool {
    reg(BRIDGE + 0x58) & 1 != 0
}
pub fn start(coverage: &[u8; ICON * ICON], color: [u8; 3], pixel_only: bool) {
    let black = core::ptr::addr_of!(_orbit_black) as usize;
    let band = core::ptr::addr_of!(_orbit_band) as usize;
    let band_bytes = if pixel_only { 8 } else { WIDTH * ICON * 2 };
    for off in (0..8).step_by(4) {
        put(DIRECT + black + off, 0);
    }
    for off in (0..band_bytes).step_by(4) {
        put(DIRECT + band + off, 0);
    }
    if !pixel_only {
        for (i, &alpha) in coverage.iter().enumerate() {
            let channel = |v: u8| (u16::from(v) * u16::from(alpha) + 127) / 255;
            let rgb = (channel(color[0]) >> 3) << 11
                | (channel(color[1]) >> 2) << 5
                | (channel(color[2]) >> 3);
            let pixel = (i / ICON) * WIDTH + (WIDTH - ICON) / 2 + i % ICON;
            unsafe { ((DIRECT + band + 2 * pixel) as *mut u16).write_volatile(rgb) }
        }
    }
    if pixel_only {
        unsafe {
            ((DIRECT + band) as *mut u16).write_volatile(0xffff);
        }
    }
    let before = if pixel_only {
        (WIDTH * (HEIGHT / 2) + WIDTH / 2) / 4
    } else {
        WIDTH * ((HEIGHT - ICON) / 2) / 4
    };
    let middle = if pixel_only { 1 } else { WIDTH * ICON / 4 };
    for i in 0..NODES {
        let (src, items, fixed) = match i % PER_FRAME {
            0 => (black, before, true),
            1 => (band, middle, false),
            _ => (black, WIDTH * HEIGHT / 4 - before - middle, true),
        };
        let desc = descriptors() + i * 64;
        for off in (0..64).step_by(4) {
            put(DIRECT + desc + off, 0);
        }
        put(DIRECT + desc, src as u32);
        put(DIRECT + desc + 8, 0x50105000); // DSI bridge FIFO, master 0
        put(DIRECT + desc + 16, (items - 1) as u32); // 64-bit items
        put(
            DIRECT + desc + 24,
            (descriptors() + 64 * ((i + 1) % NODES)) as u32 | 1,
        );
        put(DIRECT + desc + 36, BURSTS | VALID);

        put(
            DIRECT + desc + 32,
            1 | (u32::from(fixed) << 4) | (1 << 6) | (3 << 8) | (3 << 11) | (8 << 14) | (7 << 18),
        );
    }
    put(0x500e6014, reg(0x500e6014) | (1 << 13));
    put(0x500e6018, reg(0x500e6018) | (1 << 5));
    put(0x500e60c0, reg(0x500e60c0) & !(1 << 21));
    put(DMA + 0x58, 1);
    for _ in 0..10000 {
        if reg(DMA + 0x58) & 1 == 0 {
            break;
        }
    }
    put(DMA + 0x10, 3);
    put(DMA + 0x48, 0); // channel-only interrupt propagation
    put(DMA + 0x120, 15); // linked source and destination
    put(DMA + 0x124, 1 | (1 << 17) | (4 << 23) | (1 << 27)); // M2P, DSI HW handshake
    put(DMA + 0x198, u32::MAX);
    put(DMA + 0x180, FRAME_DONE | ERRORS);
    put(DMA + 0x190, 0);
    FRAMES.store(0, Ordering::Relaxed);
    RUNNING.store(true, Ordering::Relaxed);
    put(BRIDGE + 0x30, 1280 << 16 | 1524);
    put(BRIDGE + 0x34, 20 << 16 | 24);
    put(BRIDGE + 0x38, 720 << 16 | 802);
    put(BRIDGE + 0x3c, 2 << 16 | 40);
    put(BRIDGE + 0x18, 2); // rev1 input/output RGB565
    put(BRIDGE + 0x08, 256);
    put(BRIDGE + 0x8c, 768);
    put(BRIDGE + 0x0c, (1 << 31) | (WIDTH * HEIGHT / 4) as u32);
    put(BRIDGE + 0x6c, reg(BRIDGE + 0x6c) | (1 << 28));
    put(BRIDGE + 0x88, (PER_FRAME as u32) << 4);
    put(BRIDGE + 4, 1);
    put(BRIDGE + 0x54, 1);
    barrier();
    put(DMA + 0x128, descriptors() as u32 | 1);
    put(DMA + 0x18, 0x101);
    for _ in 0..100_000 {
        if reg(BRIDGE + 0x14) & 0x3fff >= 768 {
            break;
        }
    }
    crate::soc::dsi::start_stream();
    put(BRIDGE + 0x40, 720 << 4 | 1);
    put(BRIDGE + 0x44, 1);
}
