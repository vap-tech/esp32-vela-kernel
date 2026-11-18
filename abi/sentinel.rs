//! Versioned scalar mailbox protocol, no shared RAM or pointers.
//! HP owns 0,3,4,6,7,13,15; LP owns 1,2,5,8..12,14.
//! COMMAND/EVENT are doorbells, written last. LP may not republish until HP
//! acknowledges PUBLISH_SEQ via EVENT_ACK. One HP request outstanding.
// HP and LP intentionally consume different subsets of this wire-only module.
#![allow(dead_code)]
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

pub const HEARTBEAT: u32 = 0x4842_0000 | VERSION;
pub const GET_STATUS: u32 = 0x5354_0000 | VERSION;
pub const STATUS: u32 = 0x5353_0000 | VERSION;
pub const BAD_ARGUMENT: u32 = 2;
pub const UNARMED: u32 = 0;
pub const HEALTHY: u32 = 1;
pub const MISSED: u32 = 2;
pub const STATUS_BYTES: usize = 24; // state,age_ms(lo/hi),calibrated_hz,beats,missed transitions

pub const HEARTBEAT_MISSED: u32 = 0x484d_0000 | VERSION;

// Additive v2 extensions; journal payloads reuse words 8..12, tagged by EVENT.
pub const DIAGNOSTIC: u32 = 0x4447_0000 | VERSION;
pub const LOG_META: u32 = 0x4c4d_0000 | VERSION;
pub const LOG_READ: u32 = 0x4c52_0000 | VERSION;
pub const LOG_METADATA: u32 = 0x4d45_0000 | VERSION;
pub const LOG_ENTRY: u32 = 0x454e_0000 | VERSION;
pub const DIAGNOSTIC_EVENT: u32 = 0x4445_0000 | VERSION;
pub const LOG_GONE: u32 = 3;
pub const LOG_UNKNOWN: u32 = 4;
pub const LOG_BUSY: u32 = 5;
pub const LOG_EXHAUSTED: u32 = 6;
pub const LOG_BYTES: usize = 24;
pub const LOG_BOOT: u16 = 1;
pub const LOG_FIRST_HEARTBEAT: u16 = 2;
pub const LOG_TIMEOUT: u16 = 3;
pub const LOG_RECOVERED: u16 = 4;
pub const LOG_HP_EVENT: u16 = 5;
// LOG_METADATA words8..12: OK,oldest,newest,overwritten,capacity.
// LOG_ENTRY/DIAGNOSTIC_EVENT words8..12:
// sequence,ticks_lo,(type<<16)|ticks_hi16,arg0,arg1. Timestamp is 48-bit
// LP ticks since Sentinel startup. No address/pointer appears on the wire.
