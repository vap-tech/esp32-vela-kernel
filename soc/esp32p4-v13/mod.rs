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

pub mod timer;
