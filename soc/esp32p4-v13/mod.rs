pub mod console;
/// Read back the cache mode configured by the SPM startup.
/// # Safety
/// Single boot hart owns cache control, no concurrent reconfiguration.
pub unsafe fn memory_ready() -> bool {
    // SAFETY: mapped P4 cache-control registers, side-effect-free readback.
    unsafe {
        crate::pac::read(crate::pac::CACHE + 8) == 0x13
            && crate::pac::read(crate::pac::CACHE + 0x274) == 0x20
            && crate::pac::read(crate::pac::CACHE + 0x278) == 0
    }
}

pub mod dma;
mod mailbox;
pub mod sentinel;
pub mod timer;

pub mod clock;
pub mod display_io;
pub mod dsi;
pub mod lp_counter;
pub mod scanout;
