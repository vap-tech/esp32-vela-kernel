//! ESP32-P4 v1.3 HP SMP bring-up, `L1-ICache1` enablement, core-local CLIC routing,
//! `FROM_CPU` inter-processor interrupts (IPI), and deadlock-free synchronous PMP shootdown.
//!
//! Register addresses match the ESP32-P4 v1.3 TRM and `hal/multicore.rs` / `hal/interrupt.rs`.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use vela_kernel::sched::HARTS;

use crate::pac::{CACHE, read, write};

/// CLIC line on both `HP0` and `HP1` used for `FROM_CPU` inter-processor interrupts.
pub const IPI_LINE: u32 = 21;

/// `FROM_CPU` software interrupt trigger base (`line 0 = HP0 -> HP1`, `line 1 = HP1 -> HP0`).
const FROM_CPU_BASE: usize = 0x500e_5010;

/// Per-hart interrupt matrix base (`core 0 = 0x500d_6000`, `core 1 = 0x500d_6800`).
const INTR_MATRIX_BASE: usize = 0x500d_6000;
const INTR_MATRIX_CORE_STRIDE: usize = 0x800;
const INTR_FROM_CPU0_OFFSET: usize = 0x13c;

/// Core-local CLIC MMIO base (`0x2080_0000` is banked per hart on ESP32-P4).
const CLIC_BASE: usize = 0x2080_0000;
const CLIC_INT_BASE: usize = 0x2080_1000;

/// ESP32-P4 v1.3 ROM `ets_set_appcpu_boot_addr` entrypoint (`0x4fc0_00a8`).
const ROM_SET_APPCPU_BOOT_ADDR: usize = 0x4fc0_00a8;

/// Set to `true` once `HP1` finishes its M-mode startup and enters the scheduler loop.
static HP1_ONLINE: AtomicBool = AtomicBool::new(false);

/// Per-hart barrier flag indicating whether the hart is currently outside U-mode
/// (`true` when in M-mode `rust_trap`, `wfi` idle, or offline; `false` only after
/// `ensure_active_space` has loaded the hart's PMP and the hart is executing U-mode).
static HART_IN_KERNEL_OR_IDLE: [AtomicBool; HARTS] = [AtomicBool::new(true), AtomicBool::new(true)];

/// Fair ticket spinlock serializing M-mode kernel state (`Runtime`) across `HP0` and `HP1`.
///
/// Because `MIE = 0` throughout `rust_trap`, a hart holding `KERNEL_LOCK` cannot be
/// preempted by an interrupt on the same hart, making the lock provably deadlock-free.
pub struct TicketLock {
    next: AtomicU32,
    serving: AtomicU32,
}

impl TicketLock {
    pub const fn new() -> Self {
        Self {
            next: AtomicU32::new(0),
            serving: AtomicU32::new(0),
        }
    }

    #[inline(always)]
    pub fn lock(&self) {
        let ticket = self.next.fetch_add(1, Ordering::Relaxed);
        while self.serving.load(Ordering::Acquire) != ticket {
            core::hint::spin_loop();
        }
    }

    #[inline(always)]
    pub fn unlock(&self) {
        let next = self.serving.load(Ordering::Relaxed).wrapping_add(1);
        self.serving.store(next, Ordering::Release);
    }
}

pub static KERNEL_LOCK: TicketLock = TicketLock::new();

#[inline(always)]
pub fn mark_hart_in_kernel(hart: usize) {
    if hart < HARTS {
        HART_IN_KERNEL_OR_IDLE[hart].store(true, Ordering::SeqCst);
    }
}

#[inline(always)]
pub fn mark_hart_in_user(hart: usize) {
    if hart < HARTS {
        HART_IN_KERNEL_OR_IDLE[hart].store(false, Ordering::SeqCst);
    }
}

#[inline(always)]
pub fn is_hart_in_kernel_or_idle(hart: usize) -> bool {
    if hart < HARTS {
        HART_IN_KERNEL_OR_IDLE[hart].load(Ordering::Acquire)
    } else {
        true
    }
}

#[inline(always)]
pub fn hp1_online() -> bool {
    HP1_ONLINE.load(Ordering::Acquire)
}

#[inline(always)]
pub fn set_hp1_online() {
    HP1_ONLINE.store(true, Ordering::Release);
}

/// The interrupt matrix is not guaranteed to be reset between firmware runs: a stale
/// peripheral source mapped to [`IPI_LINE`] (observed: source 31) keeps the level-triggered
/// CLIC line pending forever and livelocks the hart in an IPI storm. Unmap every source on
/// `core` that targets [`IPI_LINE`] except our own `FROM_CPU` source.
unsafe fn release_stale_ipi_line(core: usize, own_from_cpu: usize) {
    let base = INTR_MATRIX_BASE + core * INTR_MATRIX_CORE_STRIDE;
    let own = INTR_FROM_CPU0_OFFSET / 4 + own_from_cpu;
    for src in 0..128usize {
        if src == own {
            continue;
        }
        let reg = base + src * 4;
        unsafe {
            if read(reg) & 0x3f == IPI_LINE {
                write(reg, 0);
            }
        }
    }
}

/// Route `FROM_CPU1` (`HP1 -> HP0`) to `HP0` CLIC line [`IPI_LINE`] (`21`).
///
/// # Safety
/// Must be called on `HP0` in M-mode after `soc::timer::start` initializes `HP0`'s CLIC.
pub unsafe fn init_hp0_ipi() {
    unsafe {
        // Clear any pending FROM_CPU0 / FROM_CPU1 level before routing.
        write(FROM_CPU_BASE, 0);
        write(FROM_CPU_BASE + 4, 0);
        release_stale_ipi_line(0, 1);
        // Route FROM_CPU1 (line 1) to HP0 (core 0) CLIC line 21.
        write(INTR_MATRIX_BASE + INTR_FROM_CPU0_OFFSET + 4, IPI_LINE);
        // Enable CLIC line 21 (non-vectored, level-triggered, priority 1).
        write(CLIC_INT_BASE + (IPI_LINE as usize) * 4, 0x3fc0_0100);
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}

/// Initialize `HP1`'s core-local CLIC and route `FROM_CPU0` (`HP0 -> HP1`) to [`IPI_LINE`] (`21`).
///
/// # Safety
/// Must be called on `HP1` (`mhartid == 1`) in M-mode with interrupts masked.
pub unsafe fn init_hp1_clic_and_ipi(vector: usize) {
    unsafe {
        write(FROM_CPU_BASE, 0);
        for i in 0..48 {
            write(CLIC_INT_BASE + i * 4, 0);
        }
        write(CLIC_BASE, (read(CLIC_BASE) & !0x1e) | (3 << 1));
        write(CLIC_BASE + 8, 0x1f00_0000);
        release_stale_ipi_line(1, 0);
        // Route FROM_CPU0 (line 0) to HP1 (core 1) CLIC line 21.
        write(
            INTR_MATRIX_BASE + INTR_MATRIX_CORE_STRIDE + INTR_FROM_CPU0_OFFSET,
            IPI_LINE,
        );
        write(CLIC_INT_BASE + (IPI_LINE as usize) * 4, 0x3fc0_0100);
        core::arch::asm!("csrw mtvec, {}", in(reg) (vector | 3), options(nostack));
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}

/// Send an inter-processor interrupt (`FROM_CPU`) to `target_hart` (`0` or `1`).
#[inline(always)]
pub fn send_ipi(target_hart: usize) {
    if target_hart >= HARTS {
        return;
    }
    if target_hart == 1 && !hp1_online() {
        return;
    }
    // target_hart == 1 uses FROM_CPU0 (line 0); target_hart == 0 uses FROM_CPU1 (line 1).
    let line = if target_hart == 1 { 0 } else { 1 };
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack));
        write(FROM_CPU_BASE + line * 4, 1);
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}

/// Acknowledge a received `FROM_CPU` IPI on `local_hart` (`0` or `1`).
#[inline(always)]
pub fn acknowledge_ipi(local_hart: usize) {
    if local_hart >= HARTS {
        return;
    }
    let line = if local_hart == 1 { 0 } else { 1 };
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack));
        write(FROM_CPU_BASE + line * 4, 0);
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}

/// Enable and invalidate `HP1`'s 16 KiB `L1-ICache1` (clearing bit 1 of `CACHE + 8`
/// so `CACHE + 8` transitions from `0x12` to `0x10`, keeping `L1-DCache` bypassed).
///
/// # Safety
/// Must be called on `HP0` in M-mode before releasing `HP1` from reset.
pub unsafe fn enable_icache1() -> bool {
    unsafe {
        core::arch::asm!("fence rw, rw", options(nostack));
        // Ensure both L1-ICache0 (IBUS0, bit 0) and L1-ICache1 (IBUS1, bit 1) are unshut.
        write(CACHE, read(CACHE) & !3);
        // Bypass L1-ICache1 (bit 1) while invalidating its full tag array.
        write(CACHE + 8, read(CACHE + 8) | 2);
        write(CACHE + 0x9c, 2);
        write(CACHE + 0xa0, 0);
        write(CACHE + 0xa4, 0);
        write(CACHE + 0x98, 1);
        let mut spins = 1_000_000u32;
        while read(CACHE + 0x98) & 16 == 0 {
            spins -= 1;
            if spins == 0 {
                return false;
            }
        }
        // Enable both L1-ICache0 (bit 0 = 0) and L1-ICache1 (bit 1 = 0), keep L1-DCache bypass (0x10).
        write(CACHE + 8, read(CACHE + 8) & !2);
        core::arch::asm!("fence rw, rw", "fence.i", options(nostack));
        read(CACHE) & 3 == 0 && read(CACHE + 8) == 0x10
    }
}

/// Unstall, ungate, release reset, and boot `HP1` into `entry`, waiting boundedly
/// until `HP1` marks itself online.
///
/// # Safety
/// Must be called once on `HP0` during boot after `RUNTIME` and `init_hp0_ipi` are ready.
pub unsafe fn start_hp1(entry: unsafe extern "C" fn() -> !) -> bool {
    unsafe {
        if !enable_icache1() {
            return false;
        }
        let set_rom_entry: unsafe extern "C" fn(u32) =
            core::mem::transmute(ROM_SET_APPCPU_BOOT_ADDR);
        core::arch::asm!("fence rw, rw", "fence.i", options(nostack));
        // Unstall HP1 in PMU (0x5011_5200 bits [23:16] = 0xff).
        write(
            0x5011_5200,
            (read(0x5011_5200) & !(0xff << 16)) | (0xff << 16),
        );
        core::arch::asm!("fence iorw, iorw", options(nostack));
        // Enable HP1 clock (bit 4 of 0x500e_6014).
        write(0x500e_6014, read(0x500e_6014) | (1 << 4));
        core::arch::asm!("fence iorw, iorw", options(nostack));
        // Release HP1 core reset (clear bit 8 of 0x500e_60c0).
        write(0x500e_60c0, read(0x500e_60c0) & !(1 << 8));
        core::arch::asm!("fence iorw, iorw", options(nostack));
        set_rom_entry(entry as *const () as usize as u32);

        let mut spins = 2_000_000u32;
        while !hp1_online() {
            core::hint::spin_loop();
            spins -= 1;
            if spins == 0 {
                set_rom_entry(0);
                return false;
            }
        }
        set_rom_entry(0);
        true
    }
}
