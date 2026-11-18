#![no_std]
#![no_main]
#[path = "../../soc/esp32p4-v13/mailbox.rs"]
mod mailbox;
#[path = "../../abi/sentinel.rs"]
mod wire;
use core::arch::{asm, global_asm};

global_asm!(
    r#"
    .section .text.entry,"ax"
    .globl _start
_start:
    csrci mstatus, 8
    csrw mie, zero
    .option push
    .option norelax
    la sp, _stack_top
    la t0, _bss_start
    la t1, _bss_end
    .option pop
1:  bgeu t0, t1, 2f
    sw zero, 0(t0)
    addi t0, t0, 4
    j 1b
2:  tail sentinel_main
"#
);
#[unsafe(no_mangle)]
extern "C" fn sentinel_main() -> ! {
    // Only the mailbox is shared with HP. READY is written last to trigger its IRQ.
    unsafe {
        let epoch = mailbox::word(wire::EPOCH).read_volatile();
        mailbox::word(wire::ECHO).write_volatile(epoch);
        asm!("fence iorw, iorw", options(nostack));
        mailbox::word(wire::EVENT).write_volatile(wire::READY);
        asm!("fence iorw, iorw", options(nostack));
    }
    idle()
}
fn idle() -> ! {
    loop {
        unsafe {
            asm!("wfi", options(nostack));
        }
    }
}
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    idle()
}
