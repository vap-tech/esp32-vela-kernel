//! P4 v1.3 TRM 4.7.3: GDMA-AHB CH1, one terminal descriptor per direction.
//! Only the direct (uncached) CPU alias is used. DMA uses bus addresses.
//! The two linker reservations are excluded from BSS, PMP grants and the heap.
use crate::pac::{read, write};
use vela_kernel::task::Region;
unsafe extern "C" {
    static _dma_page: u8;
    static _dma_descriptors: u8;
}
const DIRECT: usize = 0x4000_0000;
const GDMA: usize = 0x5008_5000;
const RX_INTERRUPTS: u32 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6);
pub const LINE: u32 = 19;
pub const HALF: usize = 2048;
pub fn page() -> Region {
    Region {
        base: core::ptr::addr_of!(_dma_page) as usize + DIRECT,
        size: 2 * HALF,
    }
}
unsafe fn barrier() {
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}
/// Sole owner of CH1. Called with interrupts masked and the user mapping disabled.
pub unsafe fn start(len: usize) {
    debug_assert!(len != 0 && len <= HALF && len % 4 == 0);
    let desc = core::ptr::addr_of!(_dma_descriptors) as usize;
    let data = core::ptr::addr_of!(_dma_page) as usize;
    unsafe {
        write(0x500e6018, read(0x500e6018) | 8);
        write(0x500e60c4, read(0x500e60c4) & !2);
        write(GDMA + 0x190, 1);
        write(GDMA + 0x190, 4); // TX automatic ownership writeback
        write(GDMA + 0x130, 1);
        write(GDMA + 0x130, 1 << 4); // RX memory transfer mode
        for (i, word) in [
            (1 << 31) | (1 << 30) | ((len as u32) << 12) | len as u32,
            data as u32,
            0,
            0,
            (1 << 31) | len as u32,
            (data + HALF) as u32,
            0,
        ]
        .into_iter()
        .enumerate()
        {
            write(desc + DIRECT + i * 4, word);
        }
        write(GDMA + 0x160, 1); // dummy-1, memory-to-memory
        write(GDMA + 0x1c0, 1);
        write(GDMA + 0x3b0, (desc + 16) as u32);
        write(GDMA + 0x3bc, desc as u32);
        write(GDMA + 0x1c, 0x7f);
        write(GDMA + 0x4c, 0x3f);
        write(0x500d60e4, LINE);
        write(0x2080104c, 0x3fc00100);
        write(GDMA + 0x18, RX_INTERRUPTS);
        barrier();
        write(GDMA + 0x1a0, 2);
        write(GDMA + 0x140, 4);
        barrier();
    }
}
/// Mask and acknowledge the source, then inspect uncached hardware writeback.
pub unsafe fn finish(len: usize) -> bool {
    unsafe {
        let status = read(GDMA + 0x14);
        write(GDMA + 0x18, 0);
        write(GDMA + 0x1c, status);
        barrier();
        let desc = core::ptr::addr_of!(_dma_descriptors) as usize + DIRECT;
        let rx = read(desc + 16);
        status & RX_INTERRUPTS == 2
            && read(desc) >> 31 == 0
            && rx >> 30 == 1
            && ((rx >> 12) & 4095) as usize == len
    }
}
