pub mod clock;
pub mod console;
pub mod display_io;
pub mod dma;
pub mod dsi;
pub mod flash;
pub mod psram;
pub mod scanout;
pub mod sdcard;
pub mod sentinel;
pub mod timer;

pub use vela_lp::hw::{lp_counter, mailbox};

/// Read back the cache mode configured by the SPM startup:
/// HP0 `L1-ICache0` enabled, `L1-ICache1` + `L1-DCache` bypassed (`0x12`),
/// and `L2-Cache` reclaimed as internal SRAM.
///
/// # Safety
/// Single boot hart owns cache control, no concurrent reconfiguration.
pub unsafe fn memory_ready() -> bool {
    // SAFETY: mapped P4 cache-control registers, side-effect-free readback.
    unsafe {
        crate::pac::read(crate::pac::CACHE) & 1 == 0
            && crate::pac::read(crate::pac::CACHE + 8) == 0x12
            && crate::pac::read(crate::pac::CACHE + 0x274) == 0x20
            && crate::pac::read(crate::pac::CACHE + 0x278) == 0
    }
}

/// Invalidate HP0's 16 KiB `L1-ICache0` across `[base, base + size)` after
/// writing executable instructions via the uncached D-bus, then synchronize
/// the local instruction fetch pipeline with `fence.i`.
///
/// # Safety
/// Single active HP0 hart in M-mode with IRQs masked.
pub unsafe fn invalidate_icache(base: usize, size: usize) -> bool {
    if size == 0 {
        return true;
    }
    const LINE: usize = 64;
    const CHUNK: usize = 4096;
    let start = base & !(LINE - 1);
    let Some(end) = base
        .checked_add(size)
        .and_then(|end| end.checked_add(LINE - 1))
        .map(|end| end & !(LINE - 1))
    else {
        return false;
    };
    // SAFETY: exclusive hart0 M-mode access to P4 CACHE_SYNC registers.
    unsafe {
        core::arch::asm!("fence rw, rw", options(nostack));
        crate::pac::write(crate::pac::CACHE + 0x9c, 1);
        let mut addr = start;
        while addr < end {
            let len = (end - addr).min(CHUNK);
            crate::pac::write(crate::pac::CACHE + 0xa0, addr as u32);
            crate::pac::write(crate::pac::CACHE + 0xa4, len as u32);
            crate::pac::write(crate::pac::CACHE + 0x98, 1);
            let mut spins = 1_000_000u32;
            while crate::pac::read(crate::pac::CACHE + 0x98) & 16 == 0 {
                spins -= 1;
                if spins == 0 {
                    return false;
                }
            }
            addr += len;
        }
        core::arch::asm!("fence rw, rw", "fence.i", options(nostack));
    }
    true
}
