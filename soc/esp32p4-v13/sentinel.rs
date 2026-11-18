//! Direct LP SRAM entry (TRM v1.3 §§3.10, 20.2.2.4 and 22.3).
use super::mailbox::{BASE, word};
use crate::{
    pac::{read, write},
    sentinel_wire as wire,
};
const LP_RAM: usize = 0x5010_8000;
const BOOT_ADDR: usize = 0x5011_0018;
const LP_CLOCK: usize = 0x5012_0000;
const LP_RESET: usize = LP_CLOCK + 8;
const POWER: usize = 0x5011_5184;
const WAKE_SOURCE: usize = POWER + 8;
const WAKE: usize = 0x5011_519c;
const FORCE_STALL: u32 = 1 << 18;
const HP_WAKE: u32 = 1 << 22;
const READY_BIT: u32 = 1 << wire::EVENT;
const IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sentinel.bin"));
const _: () = assert!(!IMAGE.is_empty() && IMAGE.len() <= 32768 - 1024);
pub const LINE: u32 = 20;
fn fence() {
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}

/// Exclusive boot-time ownership; machine interrupts masked. A stalled or
/// retained LP cannot fetch partially replaced instructions.
pub unsafe fn start() -> Option<u32> {
    unsafe {
        write(LP_CLOCK, read(LP_CLOCK) | 1 << 31);
        write(POWER, read(POWER) | FORCE_STALL | 1 << 29 | 1 << 30);
        // A retained LP can already be in WFI with its local IRQs masked.
        // Reset its pipeline while force-stall is asserted before awaiting STALL_RDY.
        write(LP_RESET, 1 << 31);
        fence();
        write(WAKE_SOURCE, HP_WAKE);
        write(WAKE, 1 << 31);
        fence();
        if !(0..100_000).any(|_| read(POWER) & 2 != 0) {
            return None;
        }
        for (i, chunk) in IMAGE.chunks(4).enumerate() {
            let mut bytes = [0u8; 4];
            bytes[..chunk.len()].copy_from_slice(chunk);
            write(LP_RAM + 4 * i, u32::from_le_bytes(bytes));
        }
        write(BOOT_ADDR, LP_RAM as u32);
        write(BASE + 0x58, 0);
        word(wire::EVENT_ACK).write_volatile(0);
        let epoch = word(wire::EPOCH).read_volatile().wrapping_add(1).max(1);
        word(wire::EPOCH).write_volatile(epoch);
        write(BASE + 0x5c, 0xffff);
        write(0x500d_6010, LINE);
        write(0x2080_1050, 0x3fc00100);
        write(BASE + 0x58, READY_BIT);
        fence();
        write(LP_RESET, 1 << 31);
        fence();
        write(POWER, read(POWER) & !FORCE_STALL);
        write(WAKE, 1 << 31);
        fence();
        Some(epoch)
    }
}
/// Consume the LP doorbell only after seeing the hardware status bit.
pub unsafe fn acknowledge(epoch: Option<u32>) -> Option<wire::Reply> {
    unsafe {
        let status = read(BASE + 0x54);
        fence();
        if status & READY_BIT == 0 {
            write(BASE + 0x5c, status);
            return None;
        }
        let echo = word(wire::ECHO).read_volatile();
        let reply = wire::Reply {
            event: word(wire::EVENT).read_volatile(),
            request: word(wire::REPLY_SEQ).read_volatile(),
            result: word(wire::RESULT).read_volatile(),
            monitor: word(wire::MONITOR).read_volatile(),
            age: (u64::from(word(wire::AGE_HI).read_volatile()) << 32)
                | u64::from(word(wire::AGE_LO).read_volatile()),
            beats: word(wire::BEATS).read_volatile(),
            published: word(wire::PUBLISH_SEQ).read_volatile(),
        };
        // LP cannot overwrite payload until this acknowledgment. Clear before
        // ACK so a fresh publication after ACK cannot be erased by our clear.
        write(BASE + 0x5c, status);
        fence();
        if Some(echo) != epoch {
            return None;
        }
        word(wire::EVENT_ACK).write_volatile(reply.published);
        fence();
        Some(reply)
    }
}

/// Sole HP request owner, IRQs masked; arguments precede command doorbell.
pub unsafe fn request(seq: u32, command: u32, arg0: u32, arg1: u32) {
    unsafe {
        word(wire::ARG0).write_volatile(arg0);
        word(wire::ARG1).write_volatile(arg1);
        word(wire::REQUEST_SEQ).write_volatile(seq);
        fence();
        word(wire::COMMAND).write_volatile(command);
        fence();
    }
}
