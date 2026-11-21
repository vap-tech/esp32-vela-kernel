//! Level interrupt for the Tab5 Keyboard `INT` line (GPIO50, active low).
//!
//! `GPIO_INT0` (interrupt matrix source 74) -> HP0 CLIC line [`LINE`]. The pin is
//! level-sensitive and the keyboard holds it low while its queue is not empty, so the
//! handler masks the source first and the owning service re-arms it after draining
//! the queue (`EVENT_WAIT` / `IRQ_ACK`), exactly like the USB console RX slot.
//!
//! Register map: P4 TRM GPIO chapter / ESP-IDF `gpio_reg.h` (hw_ver1); source number from
//! ESP-IDF `soc/interrupts.h` (neighbouring sources 53, 57 and 79 match what the timer,
//! DMA and `FROM_CPU` routing in this kernel already use).
use crate::pac::{read, write};
use core::sync::atomic::{AtomicU32, Ordering};

pub const LINE: u32 = 22;
pub const PIN: usize = 50;

/// `arm()` found `INT` already low (the service reads the keyboard without blocking).
static ARM_LOW: AtomicU32 = AtomicU32::new(0);
/// `arm()` enabled the interrupt and the service is going to block.
static ARM_WAIT: AtomicU32 = AtomicU32::new(0);

/// `(arm_low, arm_wait)` counters for `/proc/interrupts`.
pub fn arm_counts() -> (u64, u64) {
    (
        u64::from(ARM_LOW.load(Ordering::Relaxed)),
        u64::from(ARM_WAIT.load(Ordering::Relaxed)),
    )
}

const GPIO: usize = 0x500e_0000;
const GPIO_IN1: usize = GPIO + 0x40; // levels of GPIO32..56
const STATUS1_W1TC: usize = GPIO + 0x58; // pending interrupt status of GPIO32..56
const PIN_REG: usize = GPIO + 0x74 + 4 * PIN;
const MATRIX_SOURCE_GPIO_INT0: usize = 0x500d_6000 + 4 * 74;
const CLIC_LINE_CTRL: usize = 0x2080_1000 + 4 * LINE as usize;

const INT_TYPE_SHIFT: u32 = 7;
const INT_TYPE_LOW_LEVEL: u32 = 4;
const INT_TYPE_MASK: u32 = 7 << INT_TYPE_SHIFT;
const INT_ENA_MASK: u32 = 0x1f << 13;
/// `INT_ENA` bit 0 (register bit 13): deliver to the `GPIO_INT0` CPU interrupt.
const INT_ENA_CPU: u32 = 1 << 13;

/// Pin level, `true` while the keyboard asserts `INT`.
///
/// # Safety
/// Reads a GPIO input register; HP0 with the kernel lock held.
pub unsafe fn asserted() -> bool {
    unsafe { read(GPIO_IN1) & (1 << (PIN - 32)) == 0 }
}

/// Make the pin an input with pull-up and route `GPIO_INT0` to the CLIC. The source starts
/// masked.
///
/// # Safety
/// Exclusive ownership of GPIO50 and CLIC line [`LINE`] on HP0 before the line is used.
pub unsafe fn route() {
    unsafe {
        crate::soc::display_io::drive(PIN, false);
        crate::soc::display_io::configure(PIN, true);
        mask();
        write(MATRIX_SOURCE_GPIO_INT0, LINE);
        // Non-vectored, level triggered, priority 1: same as the other peripheral lines.
        write(CLIC_LINE_CTRL, 0x3fc0_0100);
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}

/// Stop the pin from raising interrupts.
///
/// # Safety
/// Exclusive ownership of GPIO50's pin configuration register.
pub unsafe fn mask() {
    unsafe {
        write(PIN_REG, read(PIN_REG) & !(INT_TYPE_MASK | INT_ENA_MASK));
        clear_status();
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}

/// The pin's interrupt status is latched; clear it so a handled `INT` does not keep the
/// CLIC line pending.
///
/// # Safety
/// Writes a write-1-to-clear GPIO register; HP0 with the kernel lock held.
unsafe fn clear_status() {
    unsafe { write(STATUS1_W1TC, 1 << (PIN - 32)) }
}

/// Enable the low-level interrupt. Returns `false` (leaving it masked) when `INT` is already
/// asserted, so the caller reads the keyboard instead of blocking.
///
/// # Safety
/// Exclusive ownership of GPIO50's pin configuration register.
pub unsafe fn arm() -> bool {
    unsafe {
        let value = read(PIN_REG) & !(INT_TYPE_MASK | INT_ENA_MASK);
        clear_status();
        write(
            PIN_REG,
            value | (INT_TYPE_LOW_LEVEL << INT_TYPE_SHIFT) | INT_ENA_CPU,
        );
        core::arch::asm!("fence iorw, iorw", options(nostack));
        if asserted() {
            mask();
            ARM_LOW.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        ARM_WAIT.fetch_add(1, Ordering::Relaxed);
        true
    }
}
