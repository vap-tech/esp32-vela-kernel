#![no_std]
#![no_main]
#[path = "../../soc/esp32p4-v13/lp_counter.rs"]
mod lp_counter;
#[path = "../../soc/esp32p4-v13/mailbox.rs"]
mod mailbox;
mod watch;
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
    li t0, ((1 << 26) | (1 << 27))
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
    j lp_timer
    .rept 4
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
lp_timer:
    call timer_event
    mret
"#
);
struct Transport {
    epoch: u32,
    published: u32,
    busy: bool,
    pending: bool,
    last_request: u32,
    hz: u32,
    watch: watch::Watch,
    reported_missed: u32,
}
struct Local(core::cell::UnsafeCell<Transport>);
// SAFETY: only LP startup and non-nested LP IRQ handlers access this LP SRAM.
// The interruptible assembly WFI loop has no Rust references or live registers.
unsafe impl Sync for Local {}
static LOCAL: Local = Local(core::cell::UnsafeCell::new(Transport {
    epoch: 0,
    published: 0,
    busy: false,
    pending: false,
    last_request: 0,
    hz: 0,
    watch: watch::Watch::new(),
    reported_missed: 0,
}));
unsafe fn publish(local: &mut Transport, event: u32, request: u32, result: u32) {
    unsafe {
        local.published = local.published.checked_add(1).unwrap();
        local.busy = true;
        mailbox::word(wire::ECHO).write_volatile(local.epoch);
        mailbox::word(wire::REPLY_SEQ).write_volatile(request);
        mailbox::word(wire::RESULT).write_volatile(result);
        let now = lp_counter::now();
        local.watch.sample(now);
        let age = local.watch.age(now);
        mailbox::word(wire::MONITOR).write_volatile(local.watch.state | (local.watch.missed << 2));
        mailbox::word(wire::AGE_LO).write_volatile(age as u32);
        mailbox::word(wire::AGE_HI).write_volatile((age >> 32) as u32);
        mailbox::word(wire::BEATS).write_volatile(local.watch.beats);
        mailbox::word(wire::PUBLISH_SEQ).write_volatile(local.published);
        asm!("fence iorw, iorw", options(nostack));
        mailbox::word(wire::EVENT).write_volatile(event);
        asm!("fence iorw, iorw", options(nostack));
    }
}
#[unsafe(no_mangle)]
extern "C" fn sentinel_main() {
    // SAFETY: startup owns LP peripheral state with MIE=0.
    unsafe {
        let irq = (1u32 << wire::COMMAND) | (1 << wire::EVENT_ACK);
        ((mailbox::BASE + 0x4c) as *mut u32).write_volatile(0xffff);
        ((mailbox::BASE + 0x48) as *mut u32).write_volatile(irq);
        let local = &mut *LOCAL.0.get();
        local.epoch = mailbox::word(wire::EPOCH).read_volatile();
        local.hz = mailbox::word(wire::CLOCK_HZ).read_volatile();
        assert!((1000..=1_000_000).contains(&local.hz));
        publish(local, wire::READY, 0, wire::OK);
    }
}
#[unsafe(no_mangle)]
extern "C" fn receive_command() {
    // SAFETY: called only by the non-nested IRQ path, local state is LP-private.
    unsafe {
        let status = ((mailbox::BASE + 0x44) as *const u32).read_volatile();
        asm!("fence iorw, iorw", options(nostack));
        ((mailbox::BASE + 0x4c) as *mut u32).write_volatile(status);
        let local = &mut *LOCAL.0.get();
        if status & (1 << wire::COMMAND) != 0 {
            local.pending = true;
        }
        if local.busy && mailbox::word(wire::EVENT_ACK).read_volatile() == local.published {
            local.busy = false;
        }
        service(local);
    }
}
unsafe fn service(local: &mut Transport) {
    unsafe {
        local.watch.sample(lp_counter::now());
        if !local.busy && local.pending {
            process_command(local);
        }
        if !local.busy && local.reported_missed < local.watch.missed {
            local.reported_missed += 1;
            publish(local, wire::HEARTBEAT_MISSED, 0, wire::OK);
        }
        // Exact deadline, not a polling tick. Healthy heartbeat resets it; a
        // missed/unarmed observer masks target1 until a new heartbeat arrives.
        lp_counter::deadline(local.watch.deadline());
    }
}
unsafe fn process_command(local: &mut Transport) {
    unsafe {
        local.pending = false;
        let epoch = mailbox::word(wire::EPOCH).read_volatile();
        let sequence = mailbox::word(wire::REQUEST_SEQ).read_volatile();
        let command = mailbox::word(wire::COMMAND).read_volatile();
        if epoch != local.epoch || sequence == 0 || sequence <= local.last_request {
            return;
        }
        local.last_request = sequence;
        let arg0 = mailbox::word(wire::ARG0).read_volatile();
        let arg1 = mailbox::word(wire::ARG1).read_volatile();
        let (event, result) = match command {
            wire::HELLO if arg0 == 0 && arg1 == 0 => (wire::ACK, wire::OK),
            wire::GET_STATUS if arg0 == 0 && arg1 == 0 => (wire::STATUS, wire::OK),
            wire::HEARTBEAT if (100..=60_000).contains(&arg0) && arg1 == 0 => {
                let timeout = (u64::from(local.hz) * u64::from(arg0) + 999) / 1000;
                local.watch.heartbeat(lp_counter::now(), timeout);
                (wire::ACK, wire::OK)
            }
            wire::HELLO | wire::GET_STATUS | wire::HEARTBEAT => (wire::ACK, wire::BAD_ARGUMENT),
            _ => (wire::ACK, wire::BAD_COMMAND),
        };
        publish(local, event, sequence, result);
    }
}
#[unsafe(no_mangle)]
extern "C" fn timer_event() {
    // SAFETY: only assembly WFI can be interrupted; MIE=0 inside this handler.
    unsafe {
        lp_counter::acknowledge();
        service(&mut *LOCAL.0.get());
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
