//! Versioned scalar mailbox protocol, no shared RAM or pointers.
//! HP owns 0,3,4,6,7,13,15; LP owns 1,2,5,8..12,14.
//! COMMAND/EVENT are doorbells, written last. LP may not republish until HP
//! acknowledges PUBLISH_SEQ via EVENT_ACK. One HP request outstanding.
pub const VERSION: u32 = 2;
pub const EPOCH: usize = 0;
pub const ECHO: usize = 1;
pub const EVENT: usize = 2;
pub const REQUEST_SEQ: usize = 3;
pub const COMMAND: usize = 4;
pub const REPLY_SEQ: usize = 5;
pub const ARG0: usize = 6;
pub const ARG1: usize = 7;
pub const RESULT: usize = 8;
pub const MONITOR: usize = 9; // bits1:0 state; bits31:2 missed-transition counter
pub const AGE_LO: usize = 10;
pub const AGE_HI: usize = 11;
pub const BEATS: usize = 12;
pub const EVENT_ACK: usize = 13;
pub const PUBLISH_SEQ: usize = 14;
pub const CLOCK_HZ: usize = 15;
pub const READY: u32 = 0x564c_0000 | VERSION;
pub const HELLO: u32 = 0x4845_0000 | VERSION;
pub const ACK: u32 = 0x4143_0000 | VERSION;
pub const OK: u32 = 0;
pub const BAD_COMMAND: u32 = 1;
#[derive(Clone, Copy)]
pub struct Reply {
    pub event: u32,
    pub request: u32,
    pub result: u32,
    pub monitor: u32,
    pub age: u64,
    pub beats: u32,
    pub published: u32,
}
