//! Minimal P4 register addresses. Startup-only registers are used by entry.S.
//! Read/write require exclusive peripheral ownership; aligned 32-bit MMIO only.
pub const USB_FIFO: usize = 0x500d_2000;
pub const USB_CONFIG: usize = USB_FIFO + 4;
pub const CACHE: usize = 0x3ff1_0000;
/// # Safety
/// Address must be mapped, aligned, readable MMIO; obey register side effects.
pub unsafe fn read(address: usize) -> u32 {
    // SAFETY: caller establishes mapping and access validity. No Rust reference.
    unsafe { (address as *const u32).read_volatile() }
}
/// # Safety
/// Address must be mapped writable MMIO, with exclusive ownership and valid value.
pub unsafe fn write(address: usize, value: u32) {
    // SAFETY: caller establishes register ownership and value validity.
    unsafe { (address as *mut u32).write_volatile(value) }
}
