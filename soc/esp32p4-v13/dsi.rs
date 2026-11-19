//! Rev1 DSI host, two lanes at 960 Mb/s, internal vertical bar generator.
//! Register layout and PLL programming follow ESP-IDF v5.5.2 hw_ver1.
use crate::pac::{read, write};
const HOST: usize = 0x500a0000;
const CLK: usize = 0x500e6000;

const POWER: usize = 0x04;
const CLOCK_DIV: usize = 0x08;
const COLOR_CODING: usize = 0x10;
const PACKET_HANDLING: usize = 0x2c;
const MODE: usize = 0x34;
const VIDEO_MODE: usize = 0x38;
const VIDEO_PACKET_SIZE: usize = 0x3c;
const H_SYNC: usize = 0x48;
const H_BACK_PORCH: usize = 0x4c;
const H_LINE: usize = 0x50;
const V_SYNC: usize = 0x54;
const V_BACK_PORCH: usize = 0x58;
const V_FRONT_PORCH: usize = 0x5c;
const V_ACTIVE: usize = 0x60;
const COMMAND_MODE: usize = 0x68;
const PACKET_HEADER: usize = 0x6c;
const PACKET_DATA: usize = 0x70;
const PACKET_STATUS: usize = 0x74;
const LANE_CLOCK: usize = 0x94;
const CLOCK_TRANSITION: usize = 0x98;
const DATA_TRANSITION: usize = 0x9c;
const PHY_RESET: usize = 0xa0;
const PHY_INTERFACE: usize = 0xa4;
const PHY_STATUS: usize = 0xb0;
const PHY_TEST_CLOCK: usize = 0xb4;
const PHY_TEST_DATA: usize = 0xb8;
const PHY_READ_TIMEOUT: usize = 0xf4;

fn update(addr: usize, mask: u32, value: u32) {
    unsafe { write(addr, (read(addr) & !mask) | value) }
}
fn set(off: usize, v: u32) {
    unsafe { write(HOST + off, v) }
}
fn wait(off: usize, mask: u32, wanted: u32) -> bool {
    (0..100_000).any(|_| unsafe { read(HOST + off) & mask == wanted })
}
fn calibration(raw: u32, bits: u32, base: i32, default: i32) -> i32 {
    let sign = 1 << (bits - 1);
    let n = raw & ((sign << 1) - 1);
    if n == 0 {
        return default;
    }
    if n & sign != 0 {
        base - (n & (sign - 1)) as i32
    } else {
        base + n as i32
    }
}
pub fn power_phy() {
    // VO3 maps to ext_ldo[1] (P0_0P2A), per the rev1 HAL channel map. Choose the closest calibrated 2.5 V setting.
    let trim = unsafe { read(0x5012d050) };
    let k = calibration(trim >> 6, 8, 975, 1000);
    let vos = calibration(trim >> 14, 6, -3, 0);
    let c = calibration(trim >> 20, 6, 990, 1000);
    let mut best = i32::MAX;
    let mut setting = 0;
    for dref in 0..16 {
        let vref = if dref < 9 {
            10 + dref
        } else {
            20 + (dref - 9) * 2
        };
        for mul in 0..8 {
            let err = ((vref * k + 20 * vos) * (4000 + mul * c) - 2500 * 80000).abs();
            if err < best {
                best = err;
                setting = (dref as u32) << 28 | (mul as u32) << 23;
            }
        }
    }
    update(0x501151c4, 0xf7800000, setting | (1 << 26));
    update(0x501151c0, (1 << 14) | (7 << 9) | (3 << 7), 3 << 7);
}
pub fn start() -> Result<(), ()> {
    update(CLK + 0x18, 1 << 12, 1 << 12);
    update(CLK + 0xc0, 1 << 26, 1 << 26);
    update(CLK + 0xc0, 1 << 26, 0);
    update(CLK + 0x2c, 1 << 8, 1 << 8); // enable REF_20M upstream of the PHY mux
    update(CLK + 0x38, 3 << 30, 0); // shared 20 MHz PHY reference on rev1
    update(CLK + 0x3c, 0x3, 3);
    set(PHY_INTERFACE, 0x3f01);
    set(POWER, 1);
    set(PHY_RESET, 1);
    set(PHY_RESET, 3);
    set(PHY_RESET, 15);
    // N=1, M=48. D-PHY test interface latches address/data on clock edges.
    for (reg, val) in [
        (0x44, 0x34),
        (0x19, 0x30),
        (0x17, 0),
        (0x18, 15),
        (0x18, 0x81),
    ] {
        set(PHY_TEST_CLOCK, 0);
        set(PHY_TEST_DATA, 1 << 16 | reg);
        set(PHY_TEST_CLOCK, 2);
        set(PHY_TEST_CLOCK, 0);
        set(PHY_TEST_DATA, val);
        set(PHY_TEST_CLOCK, 2);
        set(PHY_TEST_CLOCK, 0);
    }
    if !wait(PHY_STATUS, 0x95, 0x95) {
        return Err(());
    }
    set(MODE, 1);
    set(LANE_CLOCK, 0);
    set(CLOCK_TRANSITION, 46 << 16 | 128);
    set(DATA_TRANSITION, 50 << 16 | 104);
    set(PACKET_HANDLING, 0x1d);
    set(CLOCK_DIV, 12 << 8 | 6);
    set(PHY_READ_TIMEOUT, 6000);
    set(COMMAND_MODE, 0x010f7f00); // LP DCS and generic commands
    Ok(())
}
pub fn dcs(bytes: &[u8]) -> Result<(), ()> {
    let header = match bytes {
        [] => return Err(()),
        [cmd] => 0x05 | u32::from(*cmd) << 8,
        [cmd, arg] => 0x15 | u32::from(*cmd) << 8 | u32::from(*arg) << 16,
        _ => {
            for chunk in bytes.chunks(4) {
                if !wait(PACKET_STATUS, 8, 0) {
                    return Err(());
                }
                let mut word = [0; 4];
                word[..chunk.len()].copy_from_slice(chunk);
                set(PACKET_DATA, u32::from_le_bytes(word));
            }
            0x39 | (bytes.len() as u32) << 8
        }
    };
    if !wait(PACKET_STATUS, 2, 0) {
        return Err(());
    }
    set(PACKET_HEADER, header);
    wait(PACKET_STATUS, 5, 5).then_some(()).ok_or(())
}
pub fn video(bars: bool) -> bool {
    // 240 MHz / 3 = 80 MHz DPI. 120 MHz lane byte clock, ratio 3/2.
    update(CLK + 0x28, 1 << 30, 1 << 30); // REF_240M gate
    update(CLK + 0x3c, 0xffe0, 2 << 8 | 1 << 7 | 1 << 5);
    set(COLOR_CODING, 0); // RGB565
    set(VIDEO_PACKET_SIZE, 720);
    set(H_SYNC, 3);
    set(H_BACK_PORCH, 60);
    set(H_LINE, 1203);
    set(V_SYNC, 20);
    set(V_BACK_PORCH, 24);
    set(V_FRONT_PORCH, 200);
    set(V_ACTIVE, 1280);
    set(VIDEO_MODE, u32::from(bars) << 16 | 0x3f00 | 2); // vertical bars, burst video
    set(MODE, 0);
    set(LANE_CLOCK, 3);
    wait(PHY_STATUS, 1, 1)
}
