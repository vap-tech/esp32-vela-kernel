//! SYSTIMER unit0 / target0 -> P4 interrupt matrix -> CLIC hart0 line17.
//! Register sequence from read-only reference hal/time.rs and interrupt.rs,
//! MIT copyright 2026 vitalii petrenko. No reference runtime is used.
use crate::pac::{read, write};
/// # Safety
/// Hart0 exclusive SYSTIMER/CLIC ownership, IRQs masked. 40MHz XTAL, ROM PHY kept.
/// Installs non-vectored CLIC entry; does not enable MIE until user mret.
pub unsafe fn start(vector: usize) {
    unsafe {
        // Unit0 clock, reset and epoch, same verified XTAL /2.5 =16MHz source.
        write(0x500e601c, read(0x500e601c) | (1 << 23));
        write(0x500e6098, (read(0x500e6098) & !(1 << 29)) | (1 << 30));
        write(0x500e60c4, read(0x500e60c4) | (1 << 5));
        write(0x500e60c4, read(0x500e60c4) & !(1 << 5));
        write(0x500e2000, 1 << 31);
        write(0x500e200c, 0);
        write(0x500e2010, 0);
        write(0x500e205c, 1);
        for i in 0..48 {
            write(0x20801000 + i * 4, 0);
        }
        write(0x20800000, (read(0x20800000) & !0x1e) | (3 << 1));
        write(0x20800008, 0x1f000000);
        write(0x500d60d4, 17); // SYSTIMER target0 route, level triggered
        write(0x20801044, 0x3fc00100); // enable, non-vectored, priority1
        write(0x500e2034, (1 << 30) | 160_000); // periodic 10 ms, unit0
        write(0x500e2050, 1);
        write(0x500e206c, 1);
        write(0x500e2064, 1);
        write(0x500e2000, (1 << 31) | (1 << 30) | (1 << 24));
        core::arch::asm!("csrw mtvec, {}", in(reg) (vector | 3), options(nostack));
    }
}
/// # Safety
/// IRQ handler owns target0; clear peripheral level before returning to CLIC.
pub unsafe fn acknowledge() {
    unsafe {
        write(0x500e206c, 1);
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}

/// Read elapsed 10ms units from the actual free-running hardware epoch.
/// Unlike delivered interrupt count this does not lose time under backpressure.
/// # Safety
/// Only the outermost masked handler may own the unit0 snapshot registers.
pub unsafe fn counter_16mhz() -> Option<u64> {
    unsafe {
        write(0x500e2004, (1 << 29) | (1 << 30));
        for _ in 0..10_000 {
            if read(0x500e2004) & (1 << 29) != 0 {
                let high = read(0x500e2040) & 0xf_ffff;
                let low = read(0x500e2044);
                return Some((u64::from(high) << 32) | u64::from(low));
            }
        }
        None
    }
}

/// # Safety
/// Exclusive snapshot owner, as for counter_16mhz.
pub unsafe fn uptime_ticks() -> Option<u64> {
    unsafe { counter_16mhz().map(|ticks| ticks / 160_000) }
}
