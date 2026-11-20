//! SD / SDHC / SDXC microSD driver over the Tab5 Slot 0 bus (`GPIO39..44`) in SPI mode.
//!
//! Hardware wiring on M5Stack Tab5 (ESP32-P4 v1.3):
//! - IO power domain `VDDPST_5` (`GPIO39..48`): internal PMU LDO Channel 4 (`LDO_VO4`,
//!   `ext_ldo[4]` at `0x501151d8`/`0x501151dc`) configured to 3.3 V (`tieh = 1`, `xpd = 1`).
//! - `GPIO43`: `SD_CLK` (`SCK`, push-pull output)
//! - `GPIO44`: `SD_CMD` (`MOSI`, push-pull output)
//! - `GPIO39`: `SD_D0` (`MISO`, pulled-up input)
//! - `GPIO40`: `SD_D1` (pulled-up input)
//! - `GPIO41`: `SD_D2` (pulled-up input)
//! - `GPIO42`: `SD_D3` (`CS#`, push-pull output)
//!
//! Enforces hardware CRC7 on commands (`CMD59(1)`), CCITT CRC16 verification on all
//! 512-byte sector transfers (`CMD17` / `CMD24`), and bounded retry timeouts.
use core::fmt;

use crate::{
    pac::{read, write},
    soc::display_io as io,
};

const PIN_MISO: usize = 39; // SD_D0
const PIN_D1: usize = 40; // SD_D1
const PIN_D2: usize = 41; // SD_D2
const PIN_CS: usize = 42; // SD_D3
const PIN_SCK: usize = 43; // SD_CLK
const PIN_MOSI: usize = 44; // SD_CMD

const GPIO_OUT1_W1TS: usize = 0x500e0014;
const GPIO_OUT1_W1TC: usize = 0x500e0018;
const GPIO_IN1: usize = 0x500e0040;

const BIT_MISO: u32 = 1 << (PIN_MISO - 32);
const BIT_CS: u32 = 1 << (PIN_CS - 32);
const BIT_SCK: u32 = 1 << (PIN_SCK - 32);
const BIT_MOSI: u32 = 1 << (PIN_MOSI - 32);

pub const SECTOR_BYTES: usize = 512;

// Display diagnostic codes returned by the SDMMC / asset pipeline:
pub const ERR_SD_INIT: u32 = 19;
pub const ERR_SD_READ: u32 = 20;
pub const ERR_SD_CRC: u32 = 21;
pub const ERR_SD_WRITE: u32 = 22;

#[derive(Clone, Copy)]
pub struct Probe {
    pub sdhc: bool,
    pub ocr: u32,
    pub sectors: u32,
    pub asset_lba: u32,
    pub provisioned: bool,
}

impl Probe {
    pub const fn capacity_mib(&self) -> u32 {
        self.sectors / 2048
    }
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "[sdcard] type={} ocr={:#010x} sectors={} capacity={}MiB asset_lba={} provisioned={}",
            if self.sdhc { "SDHC/SDXC" } else { "SDSC" },
            self.ocr,
            self.sectors,
            self.capacity_mib(),
            self.asset_lba,
            u8::from(self.provisioned),
        )
    }
}

#[derive(Clone, Copy)]
pub struct Telemetry {
    pub ready: bool,
    pub sdhc: bool,
    pub ocr: u32,
    pub sectors: u32,
    pub read_sectors: u32,
    pub written_sectors: u32,
    pub errors: u32,
}

struct State {
    ready: bool,
    sdhc: bool,
    ocr: u32,
    sectors: u32,
    read_sectors: u32,
    written_sectors: u32,
    errors: u32,
}

static mut STATE: State = State {
    ready: false,
    sdhc: false,
    ocr: 0,
    sectors: 0,
    read_sectors: 0,
    written_sectors: 0,
    errors: 0,
};

/// Return a snapshot of microSD state and transfer counters.
///
/// # Safety
/// Called on HP0 with machine interrupts masked.
pub unsafe fn telemetry() -> Telemetry {
    unsafe {
        Telemetry {
            ready: STATE.ready,
            sdhc: STATE.sdhc,
            ocr: STATE.ocr,
            sectors: STATE.sectors,
            read_sectors: STATE.read_sectors,
            written_sectors: STATE.written_sectors,
            errors: STATE.errors,
        }
    }
}

unsafe fn update(addr: usize, mask: u32, value: u32) {
    unsafe { write(addr, (read(addr) & !mask) | value) }
}

unsafe fn delay_us(us: u32) {
    // HP ROM ets_delay_us, ESP32-P4 rev1.
    let delay: extern "C" fn(u32) = unsafe { core::mem::transmute(0x4fc0003cusize) };
    delay(us);
}

/// Enable internal PMU `LDO_VO4` (`ext_ldo[4]`, `PMU_EXT_LDO_P1_0P2A`) at 3.3 V
/// to power `VDDPST_5` (`GPIO39..48`) and the Tab5 microSD card slot.
unsafe fn power_ldo_vo4() {
    unsafe {
        update(0x501151dc, 0xf7800000, (15 << 28) | (7 << 23) | (1 << 26));
        update(
            0x501151d8,
            (1 << 14) | (7 << 9) | (3 << 7),
            (1 << 14) | (3 << 7),
        );
        delay_us(2500);
    }
}

unsafe fn configure_pins() {
    unsafe {
        // Inputs with internal pull-ups on D0 (MISO), D1, D2.
        for pin in [PIN_MISO, PIN_D1, PIN_D2] {
            io::drive(pin, false);
            io::latch(pin, true);
            io::configure(pin, true);
        }
        // Push-pull outputs on CS# (idle high), MOSI (idle high), SCK (idle low, Mode 0).
        io::latch(PIN_CS, true);
        io::configure(PIN_CS, true);
        io::drive(PIN_CS, true);

        io::latch(PIN_MOSI, true);
        io::configure(PIN_MOSI, true);
        io::drive(PIN_MOSI, true);

        io::latch(PIN_SCK, false);
        io::configure(PIN_SCK, false);
        io::drive(PIN_SCK, true);
    }
}

#[inline(always)]
unsafe fn cs_select() {
    unsafe {
        write(GPIO_OUT1_W1TC, BIT_CS);
    }
}

#[inline(always)]
unsafe fn cs_deselect() {
    unsafe {
        write(GPIO_OUT1_W1TS, BIT_CS | BIT_MOSI);
    }
}

/// Slow (~250 kHz) SPI Mode 0 byte exchange used only during 400 kHz card identification.
unsafe fn xfer_slow(tx: u8) -> u8 {
    let mut rx = 0u8;
    unsafe {
        for shift in (0..8).rev() {
            if (tx >> shift) & 1 != 0 {
                write(GPIO_OUT1_W1TS, BIT_MOSI);
            } else {
                write(GPIO_OUT1_W1TC, BIT_MOSI);
            }
            delay_us(2);
            write(GPIO_OUT1_W1TS, BIT_SCK);
            delay_us(2);
            if read(GPIO_IN1) & BIT_MISO != 0 {
                rx |= 1 << shift;
            }
            write(GPIO_OUT1_W1TC, BIT_SCK);
        }
        write(GPIO_OUT1_W1TS, BIT_MOSI);
    }
    rx
}

/// Fast push-pull SPI Mode 0 byte exchange for post-init commands and data tokens.
#[inline(always)]
unsafe fn xfer_fast(tx: u8) -> u8 {
    let mut rx = 0u8;
    unsafe {
        for shift in (0..8).rev() {
            if (tx >> shift) & 1 != 0 {
                write(GPIO_OUT1_W1TS, BIT_MOSI);
            } else {
                write(GPIO_OUT1_W1TC, BIT_MOSI);
            }
            write(GPIO_OUT1_W1TS, BIT_SCK);
            if read(GPIO_IN1) & BIT_MISO != 0 {
                rx |= 1 << shift;
            }
            write(GPIO_OUT1_W1TC, BIT_SCK);
        }
    }
    rx
}

/// Fast receive of `0xff` dummy MOSI bytes (MOSI kept latched high for the entire burst).
#[inline(always)]
unsafe fn recv_fast() -> u8 {
    let mut rx = 0u8;
    unsafe {
        for shift in (0..8).rev() {
            write(GPIO_OUT1_W1TS, BIT_SCK);
            if read(GPIO_IN1) & BIT_MISO != 0 {
                rx |= 1 << shift;
            }
            write(GPIO_OUT1_W1TC, BIT_SCK);
        }
    }
    rx
}

#[inline(always)]
unsafe fn xfer(tx: u8, slow: bool) -> u8 {
    unsafe { if slow { xfer_slow(tx) } else { xfer_fast(tx) } }
}

fn crc7(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in bytes {
        let mut data = b;
        for _ in 0..8 {
            crc <<= 1;
            if ((data ^ crc) & 0x80) != 0 {
                crc ^= 0x09;
            }
            data <<= 1;
        }
    }
    (crc << 1) | 1
}

fn crc16_ccitt(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in bytes {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            if (crc & 0x8000) != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

unsafe fn wait_not_busy(slow: bool, max_bytes: usize) -> bool {
    unsafe {
        for _ in 0..max_bytes {
            if xfer(0xff, slow) == 0xff {
                return true;
            }
        }
    }
    false
}

unsafe fn send_cmd(cmd: u8, arg: u32, slow: bool) -> Result<u8, ()> {
    unsafe {
        let _ = xfer(0xff, slow);
        if cmd != 0 && !wait_not_busy(slow, 50_000) {
            return Err(());
        }
        let packet = [
            0x40 | (cmd & 0x3f),
            (arg >> 24) as u8,
            (arg >> 16) as u8,
            (arg >> 8) as u8,
            arg as u8,
        ];
        let crc = crc7(&packet);
        for &b in &packet {
            let _ = xfer(b, slow);
        }
        let _ = xfer(crc, slow);
        for _ in 0..16 {
            let r1 = xfer(0xff, slow);
            if (r1 & 0x80) == 0 {
                return Ok(r1);
            }
        }
        Err(())
    }
}

/// Extract total 512-byte sector count from a 16-byte CSD v1.0 or v2.0 register.
fn parse_csd_sectors(csd: &[u8; 16]) -> u32 {
    let structure = csd[0] >> 6;
    match structure {
        // CSD Version 2.0 (SDHC / SDXC): C_SIZE is bits [69:48] -> (C_SIZE + 1) * 1024 sectors.
        1 => {
            let c_size =
                (u32::from(csd[7] & 0x3f) << 16) | (u32::from(csd[8]) << 8) | u32::from(csd[9]);
            (c_size + 1).saturating_mul(1024)
        }
        // CSD Version 1.0 (SDSC): C_SIZE [73:62], C_SIZE_MULT [49:47], READ_BL_LEN [83:80].
        0 => {
            let read_bl_len = u32::from(csd[5] & 0x0f);
            let c_size = (u32::from(csd[6] & 0x03) << 10)
                | (u32::from(csd[7]) << 2)
                | u32::from(csd[8] >> 6);
            let c_size_mult = (u32::from(csd[9] & 0x03) << 1) | u32::from(csd[10] >> 7);
            let blocknr = (c_size + 1) << (c_size_mult + 2);
            let block_len = 1u32 << read_bl_len;
            (blocknr.saturating_mul(block_len)) / 512
        }
        _ => 0,
    }
}

unsafe fn read_csd(slow: bool) -> Result<[u8; 16], ()> {
    unsafe {
        cs_select();
        let res = (|| {
            let r1 = send_cmd(9, 0, slow)?;
            if r1 != 0x00 {
                return Err(());
            }
            let mut token = 0xff;
            for _ in 0..20_000 {
                token = xfer(0xff, slow);
                if token != 0xff {
                    break;
                }
            }
            if token != 0xfe {
                return Err(());
            }
            let mut csd = [0u8; 16];
            for slot in &mut csd {
                *slot = xfer(0xff, slow);
            }
            let rx_crc = (u16::from(xfer(0xff, slow)) << 8) | u16::from(xfer(0xff, slow));
            if crc16_ccitt(&csd) != rx_crc {
                return Err(());
            }
            Ok(csd)
        })();
        cs_deselect();
        let _ = xfer(0xff, slow);
        res
    }
}

/// Initialize the Tab5 microSD card if not already initialized.
///
/// # Safety
/// Exclusive HP0 ownership of `LDO_VO4` and `GPIO39..44` with machine interrupts masked.
pub unsafe fn ensure_initialized() -> Result<(bool, bool, u32, u32), u32> {
    unsafe {
        if STATE.ready {
            return Ok((false, STATE.sdhc, STATE.ocr, STATE.sectors));
        }
        let init_res = (|| {
            power_ldo_vo4();
            configure_pins();

            // Send >= 80 dummy clock cycles with CS# and MOSI high.
            cs_deselect();
            for _ in 0..12 {
                let _ = xfer_slow(0xff);
            }

            // CMD0: GO_IDLE_STATE -> R1 = 0x01.
            let mut idle_ok = false;
            for _ in 0..8 {
                cs_select();
                let r1 = send_cmd(0, 0, true);
                cs_deselect();
                let _ = xfer_slow(0xff);
                if r1 == Ok(0x01) {
                    idle_ok = true;
                    break;
                }
                delay_us(1000);
            }
            if !idle_ok {
                return Err(ERR_SD_INIT);
            }

            // CMD8: SEND_IF_COND (2.7-3.6V, check pattern 0xAA).
            cs_select();
            let r1 = send_cmd(8, 0x0000_01aa, true).map_err(|()| {
                cs_deselect();
                ERR_SD_INIT
            })?;
            let mut r7 = 0u32;
            if r1 == 0x01 {
                for _ in 0..4 {
                    r7 = (r7 << 8) | u32::from(xfer_slow(0xff));
                }
            }
            cs_deselect();
            let _ = xfer_slow(0xff);
            let v2_card = r1 == 0x01;
            if v2_card && (r7 & 0xfff) != 0x1aa {
                return Err(ERR_SD_INIT);
            }

            // CMD59: CRC_ON_OFF(1) -> enforce CRC verification on the SD card.
            cs_select();
            let r59 = send_cmd(59, 1, true);
            cs_deselect();
            let _ = xfer_slow(0xff);
            if !matches!(r59, Ok(0x01 | 0x00)) {
                return Err(ERR_SD_INIT);
            }

            // Poll CMD55 + ACMD41(HCS) until R1 == 0x00 (bounded to ~1 s).
            let acmd41_arg = if v2_card { 0x4000_0000 } else { 0 };
            let mut ready = false;
            for _ in 0..500 {
                cs_select();
                let r55 = send_cmd(55, 0, true);
                cs_deselect();
                let _ = xfer_slow(0xff);
                if !matches!(r55, Ok(0x01 | 0x00)) {
                    return Err(ERR_SD_INIT);
                }

                cs_select();
                let r41 = send_cmd(41, acmd41_arg, true);
                cs_deselect();
                let _ = xfer_slow(0xff);
                match r41 {
                    Ok(0x00) => {
                        ready = true;
                        break;
                    }
                    Ok(0x01) => delay_us(2000),
                    _ => return Err(ERR_SD_INIT),
                }
            }
            if !ready {
                return Err(ERR_SD_INIT);
            }

            // CMD58: READ_OCR -> check power-up status (bit 31) and CCS (bit 30).
            cs_select();
            let r58 = send_cmd(58, 0, false).map_err(|()| {
                cs_deselect();
                ERR_SD_INIT
            })?;
            let mut ocr = 0u32;
            for _ in 0..4 {
                ocr = (ocr << 8) | u32::from(xfer_fast(0xff));
            }
            cs_deselect();
            let _ = xfer_fast(0xff);
            if r58 != 0x00 || (ocr & 0x8000_0000) == 0 {
                return Err(ERR_SD_INIT);
            }
            let sdhc = (ocr & 0x4000_0000) != 0;

            // Ensure 512-byte block length for SDSC cards (CMD16).
            if !sdhc {
                cs_select();
                let r16 = send_cmd(16, SECTOR_BYTES as u32, false);
                cs_deselect();
                let _ = xfer_fast(0xff);
                if r16 != Ok(0x00) {
                    return Err(ERR_SD_INIT);
                }
            }

            let csd = read_csd(false).map_err(|()| ERR_SD_INIT)?;
            let sectors = parse_csd_sectors(&csd);
            if sectors == 0 {
                return Err(ERR_SD_INIT);
            }

            STATE.ready = true;
            STATE.sdhc = sdhc;
            STATE.ocr = ocr;
            STATE.sectors = sectors;
            Ok((true, sdhc, ocr, sectors))
        })();
        if init_res.is_err() {
            STATE.errors = STATE.errors.saturating_add(1);
        }
        init_res
    }
}

/// Read one 512-byte sector at `lba` using `CMD17` and verify its CCITT CRC16.
///
/// # Safety
/// Exclusive HP0 ownership of `GPIO39..44` with machine interrupts masked.
pub unsafe fn read_sector(lba: u32, out: &mut [u8; SECTOR_BYTES]) -> Result<(), u32> {
    unsafe {
        if !STATE.ready {
            let _ = ensure_initialized()?;
        }
        let arg = if STATE.sdhc {
            lba
        } else {
            lba.checked_mul(SECTOR_BYTES as u32).ok_or(ERR_SD_READ)?
        };
        cs_select();
        let res = (|| {
            let r1 = send_cmd(17, arg, false).map_err(|()| ERR_SD_READ)?;
            if r1 != 0x00 {
                return Err(ERR_SD_READ);
            }
            write(GPIO_OUT1_W1TS, BIT_MOSI);
            let mut token = 0xff;
            for _ in 0..100_000 {
                token = recv_fast();
                if token != 0xff {
                    break;
                }
            }
            if token != 0xfe {
                return Err(ERR_SD_READ);
            }
            for slot in out.iter_mut() {
                *slot = recv_fast();
            }
            let rx_crc = (u16::from(recv_fast()) << 8) | u16::from(recv_fast());
            if crc16_ccitt(out) != rx_crc {
                return Err(ERR_SD_CRC);
            }
            Ok(())
        })();
        cs_deselect();
        let _ = recv_fast();
        if res.is_ok() {
            STATE.read_sectors = STATE.read_sectors.saturating_add(1);
        } else {
            STATE.errors = STATE.errors.saturating_add(1);
            STATE.ready = false;
        }
        res
    }
}

/// Write one 512-byte sector at `lba` using `CMD24` with CCITT CRC16 and bounded busy wait.
///
/// # Safety
/// Exclusive HP0 ownership of `GPIO39..44` with machine interrupts masked.
pub unsafe fn write_sector(lba: u32, data: &[u8; SECTOR_BYTES]) -> Result<(), u32> {
    unsafe {
        if !STATE.ready {
            let _ = ensure_initialized()?;
        }
        let arg = if STATE.sdhc {
            lba
        } else {
            lba.checked_mul(SECTOR_BYTES as u32).ok_or(ERR_SD_WRITE)?
        };
        let crc = crc16_ccitt(data);
        cs_select();
        let res = (|| {
            let r1 = send_cmd(24, arg, false).map_err(|()| ERR_SD_WRITE)?;
            if r1 != 0x00 {
                return Err(ERR_SD_WRITE);
            }
            let _ = xfer_fast(0xff);
            let _ = xfer_fast(0xfe);
            for &b in data {
                let _ = xfer_fast(b);
            }
            let _ = xfer_fast((crc >> 8) as u8);
            let _ = xfer_fast(crc as u8);
            let resp = xfer_fast(0xff);
            if (resp & 0x1f) != 0x05 {
                return Err(ERR_SD_WRITE);
            }
            write(GPIO_OUT1_W1TS, BIT_MOSI);
            if !wait_not_busy(false, 250_000) {
                return Err(ERR_SD_WRITE);
            }
            // Query card status via CMD13 (SEND_STATUS) to confirm no write/CRC error latched.
            let r13 = send_cmd(13, 0, false).map_err(|()| ERR_SD_WRITE)?;
            let r2 = recv_fast();
            if r13 != 0x00 || r2 != 0x00 {
                return Err(ERR_SD_WRITE);
            }
            Ok(())
        })();
        cs_deselect();
        let _ = recv_fast();
        if res.is_ok() {
            STATE.written_sectors = STATE.written_sectors.saturating_add(1);
        } else {
            STATE.errors = STATE.errors.saturating_add(1);
            STATE.ready = false;
        }
        res
    }
}
