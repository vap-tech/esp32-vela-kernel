//! RGB565 scanout on the dedicated DesignWare AXI DMA channel 0.
//! HP0 owns a four-frame ring. Each frame has a reusable black source block, an
//! incrementing image span, and the fixed black source again.
//! Orbit stores a 64-row RGB565 band. Pixel mode uses a single source word.
//! Full-screen modes use a dynamically allocated 2 MiB PSRAM framebuffer and four LLIs.
//! CPU accesses use direct aliases, DMA uses bus addresses.
use crate::pac::{read, write};
use core::fmt;

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
pub const ICON: usize = 64;
const FRAME_DONE: u32 = 2;
const ERRORS: u32 = 0x3fe0;
const VALID: u32 = 1 << 31;
const SLOTS: usize = 4;
const PER_FRAME: usize = 3;
const BURSTS: u32 = (1 << 6) | (16 << 7) | (1 << 15) | (16 << 16);

const SNAPSHOT_OFFSETS: [u16; 16] = [
    0x08, 0x10, 0x18, 0x30, 0x50, 0x100, 0x108, 0x110, 0x118, 0x120, 0x124, 0x128, 0x130, 0x180,
    0x188, 0x190,
];

#[derive(Clone, Copy)]
pub struct Snapshot {
    pub dma: [u32; 16],
    pub fifo: u32,
    pub clk: [u32; 3],
    pub lp: u32,
    pub frames: u32,
    pub underrun: bool,
}

impl fmt::Display for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (off, val) in SNAPSHOT_OFFSETS.into_iter().zip(self.dma) {
            write!(f, "{off:x}={val:x} ")?;
        }
        writeln!(
            f,
            "fifo={:x} clk={:x}/{:x}/{:x} lp={:x}",
            self.fifo, self.clk[0], self.clk[1], self.clk[2], self.lp
        )?;
        writeln!(
            f,
            "[scan] frames={} underrun={}",
            self.frames, self.underrun
        )
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StreamTelemetry {
    pub mode: u8,
    pub running: bool,
    pub frames: u32,
    pub underruns: u32,
    pub dma_errors: u32,
    pub fifo_level: u32,
}

pub struct Scanout {
    mode: u8,
    blocks: u32,
    frames: u32,
    underruns: u32,
    dma_errors: u32,
    running: bool,
}

unsafe fn reg(addr: usize) -> u32 {
    unsafe { read(addr) }
}
unsafe fn put(addr: usize, val: u32) {
    unsafe { write(addr, val) }
}
unsafe fn barrier() {
    unsafe { core::arch::asm!("fence iorw, iorw", options(nostack)) }
}
fn descriptors() -> usize {
    core::ptr::addr_of!(_orbit_lli) as usize
}

impl Scanout {
    pub const fn new() -> Self {
        Self {
            mode: 0,
            blocks: PER_FRAME as u32,
            frames: 0,
            underruns: 0,
            dma_errors: 0,
            running: false,
        }
    }

    pub const fn is_running(&self) -> bool {
        self.running
    }

    /// Read live scanout counters and (while active) DSI bridge FIFO occupancy.
    ///
    /// # Safety
    /// Exclusive ownership of DSI bridge registers under masked IRQs.
    pub unsafe fn telemetry(&self) -> StreamTelemetry {
        let fifo_level = if self.running {
            unsafe { reg(BRIDGE + 0x14) & 0x3fff }
        } else {
            0
        };
        StreamTelemetry {
            mode: self.mode,
            running: self.running,
            frames: self.frames,
            underruns: self.underruns,
            dma_errors: self.dma_errors,
            fifo_level,
        }
    }

    /// Refill consumed DW-GDMA linked-list descriptors on each timer tick.
    ///
    /// # Safety
    /// Exclusive ownership of DW-GDMA CH0, DSI bridge, and `_orbit_lli` under masked IRQs.
    pub unsafe fn tick(&mut self) {
        if !self.running {
            return;
        }
        let blocks = self.blocks as usize;
        unsafe {
            for i in 0..blocks * SLOTS {
                let ctl = DIRECT + descriptors() + 64 * i + 36;
                if reg(ctl) & VALID == 0 {
                    put(ctl, BURSTS | VALID);
                    if i % blocks == blocks - 1 {
                        let prev = self.frames;
                        self.frames = self.frames.wrapping_add(1);
                        if prev == 0 {
                            put(BRIDGE + 0x54, 1);
                        }
                    }
                }
            }
            if self.frames > 0 {
                if reg(BRIDGE + 0x58) & 1 != 0 {
                    self.underruns = self.underruns.saturating_add(1);
                    put(BRIDGE + 0x54, 1);
                }
                let err = reg(DMA + 0x188) & ERRORS;
                if err != 0 {
                    self.dma_errors = self.dma_errors.saturating_add(1);
                    put(DMA + 0x198, err);
                }
            }
            barrier();
        }
    }

    /// Stop active video scanout and reset DW-GDMA.
    ///
    /// # Safety
    /// Exclusive ownership of DW-GDMA and DSI bridge registers under masked IRQs.
    pub unsafe fn stop(&mut self) {
        if self.running {
            self.running = false;
            unsafe {
                put(BRIDGE + 0x40, 0);
                put(BRIDGE + 0x44, 1);
                // This driver exclusively owns DW-GDMA; AHB DMA is independent.
                put(0x500e60c0, reg(0x500e60c0) | (1 << 21));
                put(0x500e60c0, reg(0x500e60c0) & !(1 << 21));
            }
        }
    }

    /// # Safety
    /// Exclusive ownership of DSI bridge registers under masked IRQs.
    pub unsafe fn clear_underrun(&mut self) {
        self.underruns = 0;
        self.dma_errors = 0;
        unsafe {
            put(BRIDGE + 0x54, 1);
            put(DMA + 0x198, ERRORS);
        }
    }

    /// # Safety
    /// Exclusive ownership of DSI bridge registers under masked IRQs.
    pub unsafe fn underrun(&self) -> bool {
        unsafe { reg(BRIDGE + 0x58) & 1 != 0 }
    }

    /// Capture a side-effect-free diagnostic snapshot of DW-GDMA and bridge state.
    ///
    /// # Safety
    /// Exclusive ownership of DW-GDMA, DSI bridge, and clock registers under masked IRQs.
    pub unsafe fn snapshot(&self) -> Snapshot {
        unsafe {
            let mut dma = [0u32; 16];
            for (slot, off) in dma.iter_mut().zip(SNAPSHOT_OFFSETS) {
                *slot = reg(DMA + usize::from(off));
            }
            Snapshot {
                dma,
                fifo: reg(BRIDGE + 0x14),
                clk: [reg(0x500e6004), reg(0x500e6008), reg(0x500e600c)],
                lp: reg(0x50111040),
                frames: self.frames,
                underrun: self.underruns != 0 || self.underrun(),
            }
        }
    }

    /// Arm DW-GDMA CH0 and start continuous DSI bridge video scanout.
    /// When `mode >= 4`, `frame_bus_addr` specifies the AXI bus alias (`0x4800_0000 + offset`)
    /// of the allocated 2 MiB PSRAM framebuffer.
    ///
    /// # Safety
    /// Exclusive ownership of DW-GDMA CH0, DSI host/bridge, `_orbit_*` buffers,
    /// and (when `mode >= 4`) the mapped PSRAM framebuffer window.
    pub unsafe fn start(
        &mut self,
        coverage: &[u8; ICON * ICON],
        color: [u8; 3],
        mode: u8,
        frame_bus_addr: usize,
    ) {
        let pixel_only = mode == 3;
        let full = mode >= 4;
        let blocks = if full { 1 } else { PER_FRAME };
        self.blocks = blocks as u32;
        let black = core::ptr::addr_of!(_orbit_black) as usize;
        let band = core::ptr::addr_of!(_orbit_band) as usize;
        let band_bytes = if full {
            0
        } else if pixel_only {
            8
        } else {
            WIDTH * ICON * 2
        };
        unsafe {
            // Cover the source burst footprint. An eight-byte source exposed nearby
            // image data as vertical stripes on rev1.3 despite a clear underrun latch.
            for off in (0..if full { 0 } else { 256 }).step_by(4) {
                put(DIRECT + black + off, 0);
            }
            for off in (0..band_bytes).step_by(4) {
                put(DIRECT + band + off, 0);
            }
            if !pixel_only && !full {
                for (i, &alpha) in coverage.iter().enumerate() {
                    let channel = |v: u8| (u16::from(v) * u16::from(alpha) + 127) / 255;
                    let rgb = (channel(color[0]) >> 3) << 11
                        | (channel(color[1]) >> 2) << 5
                        | (channel(color[2]) >> 3);
                    let pixel = (i / ICON) * WIDTH + (WIDTH - ICON) / 2 + i % ICON;
                    ((DIRECT + band + 2 * pixel) as *mut u16).write_volatile(rgb);
                }
            }
            if pixel_only {
                ((DIRECT + band) as *mut u16).write_volatile(0xffff);
            }
            let before = if pixel_only {
                (WIDTH * (HEIGHT / 2) + WIDTH / 2) / 4
            } else {
                WIDTH * ((HEIGHT - ICON) / 2) / 4
            };
            let middle = if pixel_only { 1 } else { WIDTH * ICON / 4 };
            for i in 0..blocks * SLOTS {
                let (src, items, fixed) = if full {
                    (frame_bus_addr, WIDTH * HEIGHT / 4, false)
                } else {
                    match i % PER_FRAME {
                        0 => (black, before, true),
                        1 => (band, middle, false),
                        _ => (black, WIDTH * HEIGHT / 4 - before - middle, true),
                    }
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
                    (descriptors() + 64 * ((i + 1) % (blocks * SLOTS))) as u32 | 1,
                );
                put(DIRECT + desc + 36, BURSTS | VALID);
                put(
                    DIRECT + desc + 32,
                    1 | (u32::from(fixed) << 4)
                        | (1 << 6)
                        | (3 << 8)
                        | (3 << 11)
                        | ((if fixed { 3 } else { 8 }) << 14)
                        | (7 << 18),
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
            self.mode = mode;
            self.frames = 0;
            self.underruns = 0;
            self.dma_errors = 0;
            self.running = true;
            put(BRIDGE + 0x30, 1280 << 16 | 1524);
            put(BRIDGE + 0x34, 20 << 16 | 24);
            put(BRIDGE + 0x38, 720 << 16 | 802);
            put(BRIDGE + 0x3c, 2 << 16 | 40);
            put(BRIDGE + 0x18, 2); // rev1 input/output RGB565
            put(BRIDGE + 0x08, 256);
            put(BRIDGE + 0x8c, 768);
            put(BRIDGE + 0x0c, (1 << 31) | (WIDTH * HEIGHT / 4) as u32);
            put(
                BRIDGE + 0x6c,
                (reg(BRIDGE + 0x6c) & !(1 << 28)) | if blocks > 1 { 1 << 28 } else { 0 },
            );
            put(BRIDGE + 0x88, (blocks as u32) << 4);
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
    }
}
