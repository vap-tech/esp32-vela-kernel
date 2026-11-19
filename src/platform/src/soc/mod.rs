pub mod clock;
pub mod console;
pub mod display_io;
pub mod dma;
pub mod dsi;
pub mod flash;
pub mod psram;
pub mod scanout;
pub mod sentinel;
pub mod timer;

pub use vela_lp::hw::{lp_counter, mailbox};

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
