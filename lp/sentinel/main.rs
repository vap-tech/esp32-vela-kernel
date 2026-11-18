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
2:  call sentinel_main
    la t0, lp_vectors
    ori t0, t0, 1
    csrw mtvec, t0
    li t0, (1 << 26)
    csrw mie, t0
    csrsi mstatus, 8
3:  wfi
    j 3b

    .section .text.vectors,"ax"
    .balign 256
    .option push
    .option norvc
lp_vectors:
    .rept 26
    j lp_fault
    .endr
    j lp_mailbox
    .rept 5
    j lp_fault
    .endr
    .option pop
lp_fault:
    csrci mstatus, 8
4:  wfi
    j 4b
lp_mailbox:
    # Only the assembly WFI loop can be interrupted. It holds no live
    # registers. The standard Rust call preserves the stack and callee saves.
    call receive_command
    mret
"#
);
#[unsafe(no_mangle)]
extern "C" fn sentinel_main() {
    // Only the mailbox is shared with HP. READY is written last to trigger its IRQ.
    unsafe {
        let irq = 1u32 << wire::COMMAND;
        ((mailbox::BASE + 0x4c) as *mut u32).write_volatile(irq);
        ((mailbox::BASE + 0x48) as *mut u32).write_volatile(irq);
        let epoch = mailbox::word(wire::EPOCH).read_volatile();
        mailbox::word(wire::ECHO).write_volatile(epoch);
        asm!("fence iorw, iorw", options(nostack));
        mailbox::word(wire::EVENT).write_volatile(wire::READY);
        asm!("fence iorw, iorw", options(nostack));
    }
}
#[unsafe(no_mangle)]
extern "C" fn receive_command() {
    unsafe {
        let irq = 1u32 << wire::COMMAND;
        if ((mailbox::BASE + 0x44) as *const u32).read_volatile() & irq == 0 {
            return;
        }
        asm!("fence iorw, iorw", options(nostack));
        let cmd = mailbox::word(wire::COMMAND).read_volatile();
        let seq = mailbox::word(wire::REQUEST_SEQ).read_volatile();
        let epoch = mailbox::word(wire::EPOCH).read_volatile();
        ((mailbox::BASE + 0x4c) as *mut u32).write_volatile(irq);
        if cmd == wire::HELLO && seq != 0 {
            mailbox::word(wire::ECHO).write_volatile(epoch);
            mailbox::word(wire::REPLY_SEQ).write_volatile(seq);
            asm!("fence iorw, iorw", options(nostack));
            mailbox::word(wire::EVENT).write_volatile(wire::ACK);
        }
        asm!("fence iorw, iorw", options(nostack));
    }
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
