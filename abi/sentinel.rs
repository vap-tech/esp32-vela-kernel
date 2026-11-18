//! Wire constants only. HP owns word 0. LP writes the echo before the READY doorbell.
pub const EPOCH: usize = 0;
pub const ECHO: usize = 1;
pub const EVENT: usize = 2;
pub const VERSION: u32 = 1;
pub const READY: u32 = 0x564c_0000 | VERSION;
pub const REQUEST_SEQ: usize = 3;
pub const COMMAND: usize = 4;
pub const REPLY_SEQ: usize = 5;
pub const HELLO: u32 = 0x4845_0000 | VERSION;
pub const ACK: u32 = 0x4143_0000 | VERSION;
