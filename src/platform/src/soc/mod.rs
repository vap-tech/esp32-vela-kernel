pub mod clock;
pub mod console;
pub mod display_io;
pub mod dma;
pub mod dsi;
pub mod flash;
pub mod psram;
pub mod rng;
pub mod scanout;
pub mod sdcard;
pub mod sentinel;
pub mod smp;
pub mod timer;

pub use vela_lp::hw::{lp_counter, mailbox};

/// Read back the cache mode configured by the SPM startup:
/// `L1-ICache0` enabled (and `L1-ICache1` either bypassed `0x12` before HP1 start
/// or enabled `0x10` after HP1 start), `L1-DCache` bypassed, and `L2-Cache`
/// reclaimed as internal SRAM.
///
/// # Safety
/// Single boot hart owns cache control, no concurrent reconfiguration.
pub unsafe fn memory_ready() -> bool {
    // SAFETY: mapped P4 cache-control registers, side-effect-free readback.
    unsafe {
        crate::pac::read(crate::pac::CACHE) & 1 == 0
            && matches!(crate::pac::read(crate::pac::CACHE + 8), 0x10 | 0x12)
            && crate::pac::read(crate::pac::CACHE + 0x274) == 0x20
            && crate::pac::read(crate::pac::CACHE + 0x278) == 0
    }
}

/// Invalidate both `HP0`'s `L1-ICache0` and `HP1`'s `L1-ICache1` (16 KiB each)
/// across `[base, base + size)` after writing executable instructions via the
/// uncached D-bus, then synchronize the local instruction fetch pipeline with `fence.i`.
///
/// # Safety
/// Must be called under the kernel lock in M-mode with IRQs masked.
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
    let mask = if smp::hp1_online() { 3 } else { 1 };
    // SAFETY: exclusive kernel-locked M-mode access to P4 CACHE_SYNC registers.
    unsafe {
        core::arch::asm!("fence rw, rw", options(nostack));
        crate::pac::write(crate::pac::CACHE + 0x9c, mask);
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
