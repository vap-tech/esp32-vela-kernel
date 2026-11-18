//! LP TIMER target1 and 48-bit slow-clock counter, P4 v1.3 TRM LP Timer.
//! HP owns snapshots only during stalled LP bootstrap; thereafter LP exclusively.
pub const BASE: usize = 0x5011_2000;
pub const MASK: u64 = (1u64 << 48) - 1;
unsafe fn read(offset: usize) -> u32 {
    unsafe { ((BASE + offset) as *const u32).read_volatile() }
}
unsafe fn write(offset: usize, value: u32) {
    unsafe {
        ((BASE + offset) as *mut u32).write_volatile(value);
    }
}
/// # Safety
/// Exclusive LP timer snapshot owner; do not reset the free-running RTC counter.
pub unsafe fn now() -> u64 {
    unsafe {
        write(0x10, read(0x10) | (1 << 28));
        core::arch::asm!("fence iorw,iorw", options(nostack));
        u64::from(read(0x14)) | (u64::from(read(0x18) & 0xffff) << 32)
    }
}
/// # Safety
/// LP held during HP boot, owns target1 and its local interrupt. Target0 untouched.
pub unsafe fn initialize() {
    unsafe {
        write(0x3fc, read(0x3fc) | (1 << 31));
        write(0x40, read(0x40) & !(1 << 31));
        write(0x0c, read(0x0c) & 0xffff);
        write(0x44, 1 << 31);
        core::arch::asm!("fence iorw,iorw", options(nostack));
    }
}
