//! Platform DMA cache-coherency hooks and channel indices.
//!
//! Hardware state live-tracked in `vela_kernel::dma::DmaTable`:
//! - `DMA_CH_GDMA_AHB_CH1 = 0`: GDMA-AHB channel 1 memory-to-memory copy.
//! - `DMA_CH_DW_GDMA_SCANOUT = 1`: DesignWare AXI DMA channel 0 MIPI-DSI video scanout.
use vela_kernel::{dma::DmaDirection, task::Region};

use crate::syscall;

pub const DMA_CH_GDMA_AHB_CH1: usize = syscall::DMA_CHANNEL_GDMA_AHB_CH1 as usize;
pub const DMA_CH_DW_GDMA_SCANOUT: usize = syscall::DMA_CHANNEL_DW_GDMA_SCANOUT as usize;

/// Prepare `region` before handing ownership to a DMA bus master.
/// While `L1-DCache` remains in bypass mode (`0x12`), enforces a full I/O and
/// memory ordering fence; when cached `L1-DCache` mappings are enabled in a later
/// stage, this hook performs a cache-line clean/writeback for `ToDevice`/`MemToMem`
/// and invalidate for `FromDevice`.
#[inline(always)]
pub fn prepare_dma_buffer(_region: Region, _direction: DmaDirection, _cached: bool) {
    // SAFETY: memory ordering barrier before programming DMA descriptors.
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}

/// Complete `region` after a DMA bus master finishes and before restoring U-mode access.
/// While `L1-DCache` remains in bypass mode, enforces a full I/O and memory ordering
/// fence; when cached `L1-DCache` mappings are enabled, this hook invalidates stale
/// `L1-DCache` lines for `FromDevice`/`MemToMem`.
#[inline(always)]
pub fn finish_dma_buffer(_region: Region, _direction: DmaDirection, _cached: bool) {
    // SAFETY: memory ordering barrier after DMA completion IRQ.
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}
