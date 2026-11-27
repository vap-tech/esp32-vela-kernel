//! Hardware 4-bit SDMMC Host Controller (`SDHOST` at `0x5008_3000`, DesignWare DWC-MSHC)
//! with internal descriptor DMA (`IDMAC`) over Tab5 Slot 0 (`GPIO39..44`), plus automatic
//! fallback to SPI mode (`CMD59` CRC7 + CCITT CRC16) if a card rejects native SD bus init.
//!
//! Hardware wiring on M5Stack Tab5 (`ESP32-P4 v1.3`):
//! - IO power domain `VDDPST_5` (`GPIO39..48`): internal PMU LDO Channel 4 (`LDO_VO4`,
//!   `ext_ldo[4]` at `0x5011_51d8`/`0x5011_51dc`) configured to 3.3 V (`tieh = 1`, `xpd = 1`).
//! - Slot 0 dedicated IOMUX Function 0 (`MCU_SEL = 0`, `FUN_DRV = 3`, `FUN_IE = 1`):
//!   - `GPIO43`: `SD_CLK` (card clock output, up to 40 MHz High-Speed / 20 MHz Default-Speed)
//!   - `GPIO44`: `SD_CMD` (command line, internal pull-up enabled)
//!   - `GPIO39`: `SD_D0` (data bit 0, internal pull-up enabled)
//!   - `GPIO40`: `SD_D1` (data bit 1, internal pull-up enabled)
//!   - `GPIO41`: `SD_D2` (data bit 2, internal pull-up enabled)
//!   - `GPIO42`: `SD_D3` (data bit 3, internal pull-up enabled)
//!
//! Features:
//! - Full 4-bit SD / SDHC / SDXC native bus identification (`CMD0`, `CMD8`, `CMD55`+`ACMD41`,
//!   `CMD2`, `CMD3`, `CMD9`, `CMD7`, `CMD16(512)`, `ACMD42(0)`, `ACMD6(2)` 4-bit width,
//!   and `CMD6` High-Speed `40 MHz` switch from `PLL_160M / 4`).
//! - Chained 8-descriptor IDMAC engine (`8 × 4096 B = 32 KiB` = `64` sectors per multi-block
//!   `CMD18` read / `CMD25` write with hardware `send_auto_stop` `CMD12` and CRC verification).
use core::fmt;

use crate::{
    pac::{read, write},
    soc::display_io as io,
};

const PIN_D0: usize = 39; // SD_D0 / MISO
const PIN_D1: usize = 40; // SD_D1
const PIN_D2: usize = 41; // SD_D2
const PIN_D3: usize = 42; // SD_D3 / CS#
const PIN_CLK: usize = 43; // SD_CLK / SCK
const PIN_CMD: usize = 44; // SD_CMD / MOSI

const GPIO_BASE: usize = 0x500e_0000;
const GPIO_OUT1_W1TS: usize = 0x500e_0014;
const GPIO_OUT1_W1TC: usize = 0x500e_0018;
const GPIO_IN1: usize = 0x500e_0040;
const IO_MUX_BASE: usize = 0x500e_1000;

const BIT_MISO: u32 = 1 << (PIN_D0 - 32);
const BIT_CS: u32 = 1 << (PIN_D3 - 32);
const BIT_SCK: u32 = 1 << (PIN_CLK - 32);
const BIT_MOSI: u32 = 1 << (PIN_CMD - 32);

// Clocks & Reset (`HP_SYS_CLKRST` at `0x500e_6000` and `LP_AON_CLKRST` at `0x5011_1000`)
const HP_SYS_CLKRST: usize = 0x500e_6000;
const SOC_CLK_CTRL1_REG: usize = HP_SYS_CLKRST + 0x018;
const REF_CLK_CTRL2_REG: usize = HP_SYS_CLKRST + 0x02c;
const PERI_CLK_CTRL01_REG: usize = HP_SYS_CLKRST + 0x034;
const PERI_CLK_CTRL02_REG: usize = HP_SYS_CLKRST + 0x038;

const LP_CLKRST_SDMMC_RST_REG: usize = 0x5011_104c;
const LP_CLKRST_RST_EN_SDMMC: u32 = 1 << 28;

// DesignWare Mobile Storage Host Controller (`SDHOST` at `0x5008_3000`)
const SDHOST: usize = 0x5008_3000;
const SDHOST_CTRL: usize = SDHOST + 0x000;
const SDHOST_CLKDIV: usize = SDHOST + 0x008;
const SDHOST_CLKSRC: usize = SDHOST + 0x00c;
const SDHOST_CLKENA: usize = SDHOST + 0x010;
const SDHOST_TMOUT: usize = SDHOST + 0x014;
const SDHOST_CTYPE: usize = SDHOST + 0x018;
const SDHOST_BLKSIZ: usize = SDHOST + 0x01c;
const SDHOST_BYTCNT: usize = SDHOST + 0x020;
const SDHOST_INTMASK: usize = SDHOST + 0x024;
const SDHOST_CMDARG: usize = SDHOST + 0x028;
const SDHOST_CMD: usize = SDHOST + 0x02c;
const SDHOST_RESP0: usize = SDHOST + 0x030;
const SDHOST_RESP1: usize = SDHOST + 0x034;
const SDHOST_RESP2: usize = SDHOST + 0x038;
const SDHOST_RESP3: usize = SDHOST + 0x03c;
const SDHOST_RINTSTS: usize = SDHOST + 0x044;
const SDHOST_STATUS: usize = SDHOST + 0x048;
const SDHOST_FIFOTH: usize = SDHOST + 0x04c;
const SDHOST_BMOD: usize = SDHOST + 0x080;
const SDHOST_PLDMND: usize = SDHOST + 0x084;
const SDHOST_DBADDR: usize = SDHOST + 0x088;
const SDHOST_IDSTS: usize = SDHOST + 0x08c;
const IDSTS_DESCRIPTOR_UNAVAILABLE: u32 = 1 << 4;
const SDHOST_IDINTEN: usize = SDHOST + 0x090;
const SDHOST_CARDTHRCTL: usize = SDHOST + 0x100;

// SDHOST_CTRL bits
const CTRL_CONTROLLER_RESET: u32 = 1 << 0;
const CTRL_FIFO_RESET: u32 = 1 << 1;
const CTRL_DMA_RESET: u32 = 1 << 2;
const CTRL_ALL_RESET: u32 = CTRL_CONTROLLER_RESET | CTRL_FIFO_RESET | CTRL_DMA_RESET;
const CTRL_INT_ENABLE: u32 = 1 << 4;
const CTRL_DMA_ENABLE: u32 = 1 << 5;
const CTRL_USE_INTERNAL_DMA: u32 = 1 << 25;

// SDHOST_BMOD bits
const BMOD_SWR: u32 = 1 << 0;
const BMOD_FB: u32 = 1 << 1;
const BMOD_DE: u32 = 1 << 7;

// SDHOST_CMD bits
const CMD_RESP_EXPECT: u32 = 1 << 6;
const CMD_RESP_LONG: u32 = 1 << 7;
const CMD_CHECK_CRC: u32 = 1 << 8;
const CMD_DATA_EXPECTED: u32 = 1 << 9;
const CMD_RW_WRITE: u32 = 1 << 10;
const CMD_SEND_AUTO_STOP: u32 = 1 << 12;
const CMD_WAIT_PRVDATA: u32 = 1 << 13;
const CMD_STOP_ABORT: u32 = 1 << 14;
const CMD_SEND_INIT: u32 = 1 << 15;
const CMD_UPDATE_CLK_ONLY: u32 = 1 << 21;
const CMD_USE_HOLD_REG: u32 = 1 << 29;
const CMD_START: u32 = 1 << 31;

// SDHOST_RINTSTS bits
const INT_RE: u32 = 1 << 1;
const INT_CMD_DONE: u32 = 1 << 2;
const INT_DTO: u32 = 1 << 3;
const INT_RCRC: u32 = 1 << 6;
const INT_DCRC: u32 = 1 << 7;
const INT_RTO: u32 = 1 << 8;
const INT_DRTO: u32 = 1 << 9;
const INT_HTO: u32 = 1 << 10;
const INT_FRUN: u32 = 1 << 11;
const INT_HLE: u32 = 1 << 12;
const INT_SBE: u32 = 1 << 13;
const INT_ACD: u32 = 1 << 14;
const INT_EBE: u32 = 1 << 15;

const CMD_ERR_MASK: u32 = INT_RE | INT_RCRC | INT_RTO | INT_HLE;
const DATA_ERR_MASK: u32 = INT_DCRC | INT_DRTO | INT_HTO | INT_FRUN | INT_SBE | INT_EBE;

// SDHOST_STATUS bits
const STATUS_DATA_BUSY: u32 = 1 << 9;
const STATUS_DATA_SM_BUSY: u32 = 1 << 10;

// IDMAC Descriptor flags (DES0)
const DES0_DIC: u32 = 1 << 1;
const DES0_LD: u32 = 1 << 2;
const DES0_FD: u32 = 1 << 3;
const DES0_CH: u32 = 1 << 4;
const DES0_OWN: u32 = 1 << 31;

pub const SECTOR_BYTES: usize = 512;
const BENCH_CHUNK_SECTORS: usize = 64; // 32 KiB per multi-block CMD18/CMD25 IDMA burst
const BENCH_CHUNK_BYTES: usize = BENCH_CHUNK_SECTORS * SECTOR_BYTES;
const IDMA_DESC_MAX_BYTES: usize = 4096;
/// Descriptor ring capacity: one 1 MiB streaming command = 256 x 4 KiB descriptors.
const IDMA_DESCS: usize = 256;
/// Streaming benchmark: the 32 KiB `DMA_BUF` is a ring of `NSEG` 8 KiB segments that the CPU
/// refills (write) or verifies (read) while the IDMAC is already working on later segments.
const SEG_BYTES: usize = 8192;
const NSEG: usize = BENCH_CHUNK_BYTES / SEG_BYTES;
const DESCS_PER_SEG: usize = SEG_BYTES / IDMA_DESC_MAX_BYTES;

// Display diagnostic codes returned by the SDMMC / asset pipeline:
pub const ERR_SD_INIT: u32 = 19;
pub const ERR_SD_READ: u32 = 20;
pub const ERR_SD_CRC: u32 = 21;
pub const ERR_SD_WRITE: u32 = 22;
pub const ERR_SD_OVERRUN: u32 = 23; // CPU could not keep ahead of the IDMAC ring

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct IdmaDesc {
    des0: u32,
    des1: u32,
    des2: u32,
    des3: u32,
}

const DIRECT_UNCACHED: usize = 0x4000_0000;

unsafe extern "C" {
    static mut _sdmmc_idma_chain: [IdmaDesc; IDMA_DESCS];
    static mut _sdmmc_dma_buf: [u32; BENCH_CHUNK_BYTES / 4];
}

#[inline(always)]
fn idma_chain_bus_addr(idx: usize) -> usize {
    (unsafe { &raw const _sdmmc_idma_chain[idx] }) as usize
}

#[inline(always)]
fn idma_chain_direct_ptr(idx: usize) -> *mut IdmaDesc {
    (idma_chain_bus_addr(idx) + DIRECT_UNCACHED) as *mut IdmaDesc
}

#[inline(always)]
fn dma_buf_bus_addr() -> usize {
    (&raw const _sdmmc_dma_buf) as usize
}

#[inline(always)]
fn dma_buf_direct_ptr() -> *mut u8 {
    (dma_buf_bus_addr() + DIRECT_UNCACHED) as *mut u8
}

#[derive(Clone, Copy)]
pub struct Probe {
    pub sdhc: bool,
    pub sdmmc_4bit: bool,
    pub high_speed: bool,
    pub clk_khz: u32,
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
        f.write_str("[sdcard] bus=")?;
        f.write_str(if self.sdmmc_4bit {
            "sdmmc_4bit"
        } else {
            "spi_1bit"
        })?;
        write!(
            f,
            " clk={}kHz hs={}",
            self.clk_khz,
            u8::from(self.high_speed)
        )?;
        f.write_str(if self.sdhc {
            " type=SDHC/SDXC"
        } else {
            " type=SDSC"
        })?;
        write!(
            f,
            " ocr={:#010x} sectors={} capacity={}MiB asset_lba={} provisioned={} step={} rint={:#010x} st={:#010x} clk02={:#010x}\n",
            self.ocr,
            self.sectors,
            self.capacity_mib(),
            self.asset_lba,
            u8::from(self.provisioned),
            unsafe { STATE.dbg_step },
            unsafe { STATE.dbg_rint },
            unsafe { read(SDHOST_STATUS) },
            unsafe { read(PERI_CLK_CTRL02_REG) },
        )
    }
}

#[derive(Clone, Copy)]
pub struct Telemetry {
    pub ready: bool,
    pub sdhc: bool,
    pub sdmmc_4bit: bool,
    pub high_speed: bool,
    pub clk_khz: u32,
    pub ocr: u32,
    pub sectors: u32,
    pub read_sectors: u32,
    pub read_commands: u32,
    pub written_sectors: u32,
    pub errors: u32,
    pub bench_write_kib_s: u32,
    pub bench_read_kib_s: u32,
}

struct State {
    ready: bool,
    sdhc: bool,
    sdmmc_4bit: bool,
    high_speed: bool,
    clk_khz: u32,
    rca: u32,
    ocr: u32,
    sectors: u32,
    read_sectors: u32,
    read_commands: u32,
    written_sectors: u32,
    errors: u32,
    bench_write_kib_s: u32,
    bench_read_kib_s: u32,
    dbg_step: u32,
    dbg_rint: u32,
}

static mut STATE: State = State {
    ready: false,
    sdhc: false,
    sdmmc_4bit: false,
    high_speed: false,
    clk_khz: 0,
    rca: 0,
    ocr: 0,
    sectors: 0,
    read_sectors: 0,
    read_commands: 0,
    written_sectors: 0,
    errors: 0,
    bench_write_kib_s: 0,
    bench_read_kib_s: 0,
    dbg_step: 0,
    dbg_rint: 0,
};

#[inline(always)]
fn fence() {
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack));
    }
}

/// Return a snapshot of microSD state, bus mode, and transfer/benchmark counters.
///
/// # Safety
/// Called on HP0 with machine interrupts masked.
pub unsafe fn telemetry() -> Telemetry {
    unsafe {
        Telemetry {
            ready: STATE.ready,
            sdhc: STATE.sdhc,
            sdmmc_4bit: STATE.sdmmc_4bit,
            high_speed: STATE.high_speed,
            clk_khz: STATE.clk_khz,
            ocr: STATE.ocr,
            sectors: STATE.sectors,
            read_sectors: STATE.read_sectors,
            read_commands: STATE.read_commands,
            written_sectors: STATE.written_sectors,
            errors: STATE.errors,
            bench_write_kib_s: STATE.bench_write_kib_s,
            bench_read_kib_s: STATE.bench_read_kib_s,
        }
    }
}

unsafe fn update(addr: usize, mask: u32, value: u32) {
    unsafe { write(addr, (read(addr) & !mask) | value) }
}

unsafe fn delay_us(us: u32) {
    // HP ROM ets_delay_us, ESP32-P4 rev1.
    let delay: extern "C" fn(u32) = unsafe { core::mem::transmute(0x4fc0_003cusize) };
    delay(us);
}

/// Enable internal PMU `LDO_VO4` (`ext_ldo[4]`, `PMU_EXT_LDO_P1_0P2A`) at 3.3 V
/// to power `VDDPST_5` (`GPIO39..48`) and the Tab5 microSD card slot.
/// Note: in `PMU_EXT_LDO_P1_0P2A_ANA_REG` (`0x5011_51d8`), bit 14 is `force_tieh_sel = 0`
/// (so hardware uses `tieh = 1` bit 26 of `0x5011_51dc`), and `en_cur_lim = 3 << 7`.
unsafe fn power_ldo_vo4() {
    unsafe {
        update(0x5011_51dc, 0xf780_0000, (15 << 28) | (7 << 23) | (1 << 26));
        update(
            0x5011_51d8,
            (1 << 14) | (7 << 9) | (3 << 7),
            (1 << 14) | (3 << 7),
        );
        delay_us(2500);
    }
}

/// Power-cycle `LDO_VO4` (`0 V` -> `3.3 V`) while clamping `GPIO39..44` LOW so the
/// microSD rail discharges completely and any card previously locked in SPI mode
/// resets back to native 4-bit SD bus mode.
unsafe fn power_cycle_ldo_vo4(service: &mut impl FnMut()) {
    unsafe {
        for pin in [PIN_D0, PIN_D1, PIN_D2, PIN_D3, PIN_CLK, PIN_CMD] {
            io::latch(pin, false);
            io::configure(pin, false);
            io::drive(pin, true);
        }
        // In PMU.ext_ldo[4]:
        //   0x5011_51d8 (pmu_ext_ldo): bit 7 = force_tieh_sel, bit 8 = xpd,
        //                              bits [11:9] = tieh_sel, bit 14 = tieh (1 = 3.3V).
        //   0x5011_51dc (pmu_ext_ldo_ana): bits [25:23] = mul, bit 26 = en_vdet,
        //                                  bit 27 = en_cur_lim, bits [31:28] = dref.
        // Turn off LDO_VO4 (xpd = 0 at bit 8, tieh = 0 at bit 14) and discharge rail for 150 ms.
        update(0x5011_51d8, (1 << 14) | (7 << 9) | (3 << 7), 1 << 7);
        update(0x5011_51dc, 0xf780_0000, 0);
        for _ in 0..15 {
            delay_us(10_000);
            service();
        }

        // Drive D3/CS HIGH before re-applying Vdd so power-on sees native SD mode.
        io::latch(PIN_D3, true);
        power_ldo_vo4();
        delay_us(10_000);
    }
}

/// Connect a GPIO matrix input signal (`signal_idx`) to `gpio_num` (`0x3e` = const 0, `0x3f` = const 1).
unsafe fn gpio_matrix_connect_in(gpio_num: u32, signal_idx: usize, inv: bool) {
    // GPIO_FUNC1_IN_SEL_CFG_REG is at GPIO_BASE + 0x15c (so signal N is at GPIO_BASE + 0x158 + 4 * N).
    // Bit 7 = SIG_IN_SEL (1 = route through GPIO matrix), Bit 6 = IN_INV_SEL, Bits [5:0] = IN_SEL.
    let val = (1 << 7) | (u32::from(inv) << 6) | (gpio_num & 0x3f);
    unsafe {
        write(GPIO_BASE + 0x158 + 4 * signal_idx, val);
    }
}

/// Route `pin` to Slot 0 dedicated IOMUX Function 0 (`MCU_SEL = 0`, `FUN_DRV = 3`,
/// `FUN_IE = 1`, internal pull-up if `pull_up`).
unsafe fn configure_pin_sdmmc_iomux(pin: usize, pull_up: bool) {
    unsafe {
        write(GPIO_BASE + 0x074 + 4 * pin, 0);
        write(GPIO_BASE + 0x558 + 4 * pin, 0x100);
        let mux = IO_MUX_BASE + 0x004 + 4 * pin;
        // Clear MCU_SEL [14:12] -> 0 (IOMUX Func 0), FUN_DRV [11:10] -> 3,
        // FUN_IE [9] -> 1, FUN_PU [8] -> pull_up, FUN_PD [7] -> 0.
        let val = (read(mux) & !0x7f80) | (3 << 10) | (1 << 9) | (u32::from(pull_up) << 8);
        write(mux, val);
    }
}

/// Route `GPIO43` (`CLK`), `GPIO44` (`CMD`), `GPIO39..41` (`D0..D2`) to IOMUX Func 0,
/// while holding `GPIO42` (`D3/CS`) driven high as a GPIO output during `CMD0` so the card
/// enters native SD bus mode (even if previously initialized in SPI mode).
unsafe fn configure_pins_sdmmc_init() {
    unsafe {
        for (pin, pull_up) in [
            (PIN_D0, true),
            (PIN_D1, true),
            (PIN_D2, true),
            (PIN_CLK, false),
            (PIN_CMD, true),
        ] {
            configure_pin_sdmmc_iomux(pin, pull_up);
        }
        // Hold D3 (GPIO42) actively HIGH via GPIO output during CMD0..ACMD6.
        io::latch(PIN_D3, true);
        io::configure(PIN_D3, true);
        io::drive(PIN_D3, true);

        // Route Slot 0 auxiliary inputs in GPIO matrix (matching ESP-IDF sdmmc_host_init_slot):
        //   SD_CARD_DETECT_N_1_PAD_IN_IDX (126) <- CONST_ZERO (0x3e, card present)
        //   SD_CARD_INT_N_1_PAD_IN_IDX    (128) <- CONST_ONE  (0x3f, no SDIO IRQ)
        //   SD_CARD_WRITE_PRT_1_PAD_IN_IDX(130) <- CONST_ONE  (0x3f, inverted -> 0 = not write-protected)
        gpio_matrix_connect_in(0x3e, 126, false);
        gpio_matrix_connect_in(0x3f, 128, false);
        gpio_matrix_connect_in(0x3f, 130, true);
    }
}

/// Latch CIU clock registers (`CLKDIV`, `CLKSRC`, `CLKENA`) into the DWC-MSHC card clock domain.
unsafe fn sdmmc_ciu_update_clock() -> Result<(), ()> {
    unsafe {
        let cmd = CMD_START | CMD_UPDATE_CLK_ONLY | CMD_WAIT_PRVDATA | CMD_USE_HOLD_REG;
        write(SDHOST_CMDARG, 0);
        write(SDHOST_CMD, cmd);
        for _ in 0..500_000 {
            if (read(SDHOST_CMD) & CMD_START) == 0 {
                return Ok(());
            }
            if (read(SDHOST_RINTSTS) & INT_HLE) != 0 {
                write(SDHOST_RINTSTS, INT_HLE);
                write(SDHOST_CMD, cmd);
            }
        }
        STATE.dbg_rint = read(SDHOST_RINTSTS);
        Err(())
    }
}

/// Configure `HP_SYS_CLKRST` (`PLL_160M` reference divided by `host_div`) before reset.
unsafe fn sdmmc_set_host_clk_div(host_div: u32) {
    unsafe {
        update(PERI_CLK_CTRL01_REG, (1 << 22) | (1 << 23), 1 << 24);
        let edge_l = (host_div - 1) & 0xf;
        let edge_h = (host_div / 2 - 1) & 0xf;
        let edge_n = (host_div - 1) & 0xf;
        let sam_sel = u32::from(host_div <= 4);
        let ctrl02_mask =
            (0xf << 9) | (0xf << 13) | (0xf << 17) | (3 << 21) | (3 << 23) | (3 << 25) | (7 << 27);
        let ctrl02_val = (edge_l << 9)
            | (edge_h << 13)
            | (edge_n << 17)
            | (1 << 23)
            | (sam_sel << 25)
            | (7 << 27);
        update(PERI_CLK_CTRL02_REG, ctrl02_mask, ctrl02_val);
        update(PERI_CLK_CTRL02_REG, 1 << 8, 1 << 8);
        update(PERI_CLK_CTRL02_REG, 1 << 8, 0);
        delay_us(10);
    }
}

/// Configure `HP_SYS_CLKRST` (`PLL_160M` reference divided by `host_div`) and
/// `SDHOST_CLKDIV` (`card_div`, dividing by `2 * card_div` when non-zero) for Slot 0.
///
/// - `400 kHz` probe: `host_div = 10` (`16 MHz`), `card_div = 20` (`÷40` -> `400 kHz`)
/// - `20 MHz` default speed: `host_div = 8` (`20 MHz`), `card_div = 0` (`÷1` -> `20 MHz`)
/// - `40 MHz` high speed: `host_div = 4` (`40 MHz`), `card_div = 0` (`÷1` -> `40 MHz`)
unsafe fn sdmmc_set_clock(host_div: u32, card_div: u32) -> Result<u32, ()> {
    unsafe {
        // 1. Gate Slot 0 card clock before changing dividers.
        write(SDHOST_CLKENA, 0);
        write(SDHOST_CLKSRC, 0);
        sdmmc_ciu_update_clock()?;

        // 2. Program host & card clock dividers and latch into CIU.
        write(SDHOST_CLKDIV, card_div & 0xff);
        sdmmc_set_host_clk_div(host_div);
        sdmmc_ciu_update_clock()?;

        // 3. Re-enable Slot 0 card clock (keep continuous clock enabled).
        write(SDHOST_CLKENA, 1);
        sdmmc_ciu_update_clock()?;

        let host_khz = 160_000 / host_div.max(1);
        let eff_khz = if card_div == 0 {
            host_khz
        } else {
            host_khz / (2 * card_div)
        };
        // Set data timeout to 100 ms worth of card clock cycles (max 0xff_ffff).
        let tmout_cycles = (eff_khz * 100).min(0x00ff_ffff);
        write(SDHOST_TMOUT, (tmout_cycles << 8) | 0xff);
        Ok(eff_khz)
    }
}

/// Wait until the card releases `SD_D0` (`DATA_BUSY` and `DATA_STATE_MC_BUSY` clear).
unsafe fn sdmmc_wait_not_busy(max_loops: u32) -> bool {
    unsafe { sdmmc_wait_not_busy_with(max_loops, &mut || {}) }
}

unsafe fn sdmmc_wait_not_busy_with(max_loops: u32, service: &mut impl FnMut()) -> bool {
    unsafe {
        for i in 0..max_loops {
            if i & 0x3fff == 0 {
                service();
            }
            if (read(SDHOST_STATUS) & (STATUS_DATA_BUSY | STATUS_DATA_SM_BUSY)) == 0 {
                return true;
            }
        }
        false
    }
}

/// Issue a non-data SD command on Slot 0 and return `(resp0, resp1, resp2, resp3)`.
unsafe fn sdmmc_send_cmd(cmd_idx: u8, arg: u32, flags: u32) -> Result<[u32; 4], ()> {
    unsafe {
        if (flags & CMD_WAIT_PRVDATA) != 0 && !sdmmc_wait_not_busy(500_000) {
            STATE.dbg_rint = 0xdead_0000 | read(SDHOST_STATUS);
            return Err(());
        }
        write(SDHOST_RINTSTS, 0xffff_ffff);
        write(SDHOST_CMDARG, arg);
        let cmd_val = u32::from(cmd_idx & 0x3f) | flags | CMD_USE_HOLD_REG | CMD_START;
        write(SDHOST_CMD, cmd_val);

        for _ in 0..500_000 {
            let rint = read(SDHOST_RINTSTS);
            if (rint & CMD_ERR_MASK) != 0 {
                STATE.dbg_rint = rint;
                write(SDHOST_RINTSTS, rint);
                return Err(());
            }
            if (rint & INT_CMD_DONE) != 0 {
                write(SDHOST_RINTSTS, INT_CMD_DONE);
                return Ok([
                    read(SDHOST_RESP0),
                    read(SDHOST_RESP1),
                    read(SDHOST_RESP2),
                    read(SDHOST_RESP3),
                ]);
            }
        }
        STATE.dbg_rint = 0xeeee_0000 | read(SDHOST_RINTSTS);
        Err(())
    }
}

/// Reset FIFO/IDMAC pointers and arm `DBADDR`, `BLKSIZ`, `BYTCNT` for the chain at `_sdmmc_idma_chain`.
unsafe fn sdmmc_arm_idma_regs(total_bytes: usize, block_size: usize) {
    unsafe {
        fence();
        // Reset FIFO & IDMA pointers cleanly before starting transfer.
        update(
            SDHOST_CTRL,
            CTRL_FIFO_RESET | CTRL_DMA_RESET,
            CTRL_FIFO_RESET | CTRL_DMA_RESET,
        );
        for _ in 0..10_000 {
            if (read(SDHOST_CTRL) & (CTRL_FIFO_RESET | CTRL_DMA_RESET)) == 0 {
                break;
            }
        }
        update(
            SDHOST_CTRL,
            CTRL_DMA_ENABLE | CTRL_USE_INTERNAL_DMA,
            CTRL_DMA_ENABLE | CTRL_USE_INTERNAL_DMA,
        );
        write(SDHOST_BMOD, BMOD_FB | BMOD_DE);
        write(SDHOST_IDSTS, 0xffff_ffff);
        write(SDHOST_RINTSTS, 0xffff_ffff);
        write(SDHOST_BLKSIZ, block_size as u32);
        write(SDHOST_BYTCNT, total_bytes as u32);
        write(SDHOST_DBADDR, idma_chain_bus_addr(0) as u32);
        write(SDHOST_PLDMND, 1);
        fence();
    }
}

/// Prepare the chained IDMAC descriptor table for `[buf_addr..buf_addr + total_bytes)`
/// and arm `SDHOST_DBADDR` / `SDHOST_BYTCNT` / `SDHOST_BLKSIZ`.
unsafe fn sdmmc_prepare_idma(
    buf_addr: usize,
    total_bytes: usize,
    block_size: usize,
) -> Result<(), ()> {
    if total_bytes == 0 || total_bytes > BENCH_CHUNK_BYTES || buf_addr % 4 != 0 {
        return Err(());
    }
    unsafe {
        let mut rem = total_bytes;
        let mut cur_addr = buf_addr;
        let mut idx = 0usize;
        while rem > 0 {
            let chunk = rem.min(IDMA_DESC_MAX_BYTES);
            let is_first = idx == 0;
            let is_last = rem == chunk;
            let mut des0 = DES0_OWN | DES0_CH;
            if is_first {
                des0 |= DES0_FD;
            }
            if is_last {
                des0 |= DES0_LD;
            } else {
                des0 |= DES0_DIC;
            }
            let next_ptr = if is_last {
                0
            } else {
                idma_chain_bus_addr(idx + 1) as u32
            };
            idma_chain_direct_ptr(idx).write_volatile(IdmaDesc {
                des0,
                des1: chunk as u32,
                des2: cur_addr as u32,
                des3: next_ptr,
            });
            cur_addr += chunk;
            rem -= chunk;
            idx += 1;
        }
        sdmmc_arm_idma_regs(total_bytes, block_size);
        Ok(())
    }
}

/// Execute an IDMAC data transfer command (`CMD6`, `CMD17`, `CMD18`, `CMD24`, `CMD25`)
/// and wait for command completion, data transfer over (`DTO`), and auto-stop (`ACD` if multi-block).
unsafe fn sdmmc_transfer_idma(
    cmd_idx: u8,
    arg: u32,
    buf_addr: usize,
    total_bytes: usize,
    block_size: usize,
    is_write: bool,
    multi_block: bool,
    service: &mut impl FnMut(),
) -> Result<(), ()> {
    unsafe {
        if !sdmmc_wait_not_busy_with(2_000_000, service) {
            return Err(());
        }
        sdmmc_prepare_idma(buf_addr, total_bytes, block_size)?;

        let mut flags = CMD_RESP_EXPECT
            | CMD_CHECK_CRC
            | CMD_DATA_EXPECTED
            | CMD_WAIT_PRVDATA
            | CMD_USE_HOLD_REG
            | CMD_START;
        if is_write {
            flags |= CMD_RW_WRITE;
        }
        if multi_block {
            flags |= CMD_SEND_AUTO_STOP;
        }
        write(SDHOST_CMDARG, arg);
        write(SDHOST_CMD, u32::from(cmd_idx & 0x3f) | flags);

        let wanted = if multi_block {
            INT_CMD_DONE | INT_DTO | INT_ACD
        } else {
            INT_CMD_DONE | INT_DTO
        };
        let mut seen = 0u32;
        for i in 0..4_000_000 {
            if i & 0x3fff == 0 {
                service();
            }
            let rint = read(SDHOST_RINTSTS);
            if (rint & (CMD_ERR_MASK | DATA_ERR_MASK)) != 0 {
                STATE.dbg_rint = rint;
                write(SDHOST_RINTSTS, 0xffff_ffff);
                let _ = sdmmc_send_cmd(12, 0, CMD_RESP_EXPECT | CMD_STOP_ABORT);
                return Err(());
            }
            seen |= rint;
            if (seen & wanted) == wanted {
                break;
            }
        }
        if (seen & wanted) != wanted {
            STATE.dbg_rint = seen;
            write(SDHOST_RINTSTS, 0xffff_ffff);
            let _ = sdmmc_send_cmd(12, 0, CMD_RESP_EXPECT | CMD_STOP_ABORT);
            return Err(());
        }
        let resp0 = read(SDHOST_RESP0);
        write(SDHOST_RINTSTS, 0xffff_ffff);
        write(SDHOST_IDSTS, 0xffff_ffff);
        fence();
        // Check R1 error bits (`0xfdf9_0000`).
        if (resp0 & 0xfdf9_0000) != 0 {
            STATE.dbg_rint = resp0;
            return Err(());
        }
        if is_write && !sdmmc_wait_not_busy_with(4_000_000, service) {
            return Err(());
        }
        Ok(())
    }
}

/// Extract total 512-byte sector count from a DWC-MSHC 136-bit R2 response (`RESP0..3`).
///
/// In DesignWare MSHC, `RESP3..0` holds CSD bits `[127:0]` (with the 8-bit CRC+end bit
/// either in `[7:0]` so `RESP3[31:30]` is `CSD_STRUCTURE [127:126]`, or shifted by 8 bits
/// on controllers that strip the 8-bit CRC). We support both alignments and cross-check.
fn parse_csd_sectors_r2(r: &[u32; 4]) -> u32 {
    // Layout A: RESP3..0 = CSD[127:0] directly (CSD_STRUCTURE = RESP3[31:30]).
    let struct_a = (r[3] >> 30) & 3;
    if struct_a == 1 {
        // CSD v2.0: C_SIZE is CSD[69:48] -> RESP2[5:0] (6 bits) | RESP1[31:16] (16 bits).
        let c_size = ((r[2] & 0x3f) << 16) | (r[1] >> 16);
        let sectors = (c_size + 1).saturating_mul(1024);
        if sectors >= 1024 {
            return sectors;
        }
    } else if struct_a == 0 {
        // CSD v1.0: READ_BL_LEN = CSD[83:80] = RESP2[19:16],
        // C_SIZE = CSD[73:62] = (RESP2[9:0] << 2) | (RESP1[31:30]),
        // C_SIZE_MULT = CSD[49:47] = RESP1[17:15].
        let read_bl_len = (r[2] >> 16) & 0xf;
        let c_size = ((r[2] & 0x3ff) << 2) | (r[1] >> 30);
        let c_size_mult = (r[1] >> 15) & 0x7;
        if (9..=11).contains(&read_bl_len) {
            let blocknr = (c_size + 1) << (c_size_mult + 2);
            let block_len = 1u32 << read_bl_len;
            let sectors = blocknr.saturating_mul(block_len) / 512;
            if sectors > 0 {
                return sectors;
            }
        }
    }
    // Layout B: RESP3..0 = CSD[127:8] (CRC stripped, shifted right by 8 bits).
    let struct_b = (r[3] >> 22) & 3;
    if struct_b == 1 {
        // CSD[69:48] is at bit [61:40] of RESP3..0 -> RESP1[29:8].
        let c_size = (r[1] >> 8) & 0x3f_ffff;
        let sectors = (c_size + 1).saturating_mul(1024);
        if sectors >= 1024 {
            return sectors;
        }
    }
    0
}

/// Initialize the hardware 4-bit SDMMC Host Controller (`0x5008_3000`) and identify the card.
unsafe fn try_init_sdmmc_4bit(
    service: &mut impl FnMut(),
) -> Result<(bool, bool, u32, u32, u32, u32), ()> {
    unsafe {
        STATE.dbg_step = 1;
        power_cycle_ldo_vo4(service);
        configure_pins_sdmmc_init();
        delay_us(500);

        // 1. Enable PLL_160M reference and SDMMC AHB/APB bus clock, pulse SDMMC reset,
        //    and start the `sdio_ls` functional clock BEFORE resetting DWC-MSHC controller.
        update(REF_CLK_CTRL2_REG, 1 << 0, 1 << 0);
        update(SOC_CLK_CTRL1_REG, 1 << 14, 1 << 14);
        update(
            LP_CLKRST_SDMMC_RST_REG,
            LP_CLKRST_RST_EN_SDMMC,
            LP_CLKRST_RST_EN_SDMMC,
        );
        delay_us(10);
        update(LP_CLKRST_SDMMC_RST_REG, LP_CLKRST_RST_EN_SDMMC, 0);
        delay_us(10);
        sdmmc_set_host_clk_div(2);

        // 2. Reset DWC-MSHC controller, FIFO, and IDMAC (requires active `sdio_ls` clock).
        STATE.dbg_step = 2;
        write(SDHOST_CTRL, CTRL_ALL_RESET);
        for _ in 0..100_000 {
            if (read(SDHOST_CTRL) & CTRL_ALL_RESET) == 0 {
                break;
            }
        }
        if (read(SDHOST_CTRL) & CTRL_ALL_RESET) != 0 {
            STATE.dbg_rint = read(SDHOST_CTRL);
            return Err(());
        }
        write(SDHOST_BMOD, BMOD_SWR);
        for _ in 0..100_000 {
            if (read(SDHOST_BMOD) & BMOD_SWR) == 0 {
                break;
            }
        }
        write(SDHOST_INTMASK, 0);
        write(SDHOST_IDINTEN, 0x37);
        write(SDHOST_RINTSTS, 0xffff_ffff);
        write(SDHOST_IDSTS, 0xffff_ffff);
        write(SDHOST_TMOUT, 0xffff_fffe);
        write(SDHOST_FIFOTH, (2 << 28) | (7 << 16) | 8);
        write(SDHOST_CARDTHRCTL, 0);
        write(SDHOST_CTYPE, 0); // start in 1-bit SD mode for identification
        write(
            SDHOST_CTRL,
            CTRL_INT_ENABLE | CTRL_DMA_ENABLE | CTRL_USE_INTERNAL_DMA,
        );
        write(SDHOST_BMOD, BMOD_FB | BMOD_DE);

        // 3. Start identification clock at 400 kHz (160 MHz / 10 / (2*20) = 400 kHz).
        STATE.dbg_step = 3;
        let _ = sdmmc_set_clock(10, 20)?;
        delay_us(2000);

        // 4. CMD0: GO_IDLE_STATE with 80 initialization clocks (`CMD_SEND_INIT`).
        //    Note: ESP-IDF does NOT set `CMD_WAIT_PRVDATA` on CMD0.
        STATE.dbg_step = 4;
        let _ = sdmmc_send_cmd(0, 0, CMD_SEND_INIT)?;
        delay_us(2000);

        // 5. CMD8: SEND_IF_COND (2.7-3.6V, check pattern 0x1AA).
        STATE.dbg_step = 5;
        let v2_card = match sdmmc_send_cmd(
            8,
            0x0000_01aa,
            CMD_RESP_EXPECT | CMD_CHECK_CRC | CMD_WAIT_PRVDATA,
        ) {
            Ok(r) => {
                if (r[0] & 0xfff) != 0x1aa {
                    return Err(());
                }
                true
            }
            Err(()) => false,
        };

        // 6. Loop CMD55(0) + ACMD41(HCS | 3.2-3.4V window) until OCR bit 31 (busy/ready) is 1.
        let acmd41_arg = if v2_card { 0x40ff_8000 } else { 0x00ff_8000 };
        let mut ocr = 0u32;
        let mut card_ready = false;
        for _ in 0..500 {
            service();
            STATE.dbg_step = 61;
            let _ = sdmmc_send_cmd(55, 0, CMD_RESP_EXPECT | CMD_CHECK_CRC | CMD_WAIT_PRVDATA)?;
            STATE.dbg_step = 62;
            // ACMD41 returns R3 (no CRC7 check, CRC field is 0b1111111).
            let r3 = sdmmc_send_cmd(41, acmd41_arg, CMD_RESP_EXPECT | CMD_WAIT_PRVDATA)?;
            ocr = r3[0];
            if (ocr & 0x8000_0000) != 0 {
                card_ready = true;
                break;
            }
            delay_us(2000);
        }
        if !card_ready {
            return Err(());
        }
        let sdhc = (ocr & 0x4000_0000) != 0;

        // 7. CMD2: ALL_SEND_CID (long 136-bit R2 response; no CRC7 check over 136-bit R2).
        STATE.dbg_step = 7;
        let _ = sdmmc_send_cmd(2, 0, CMD_RESP_EXPECT | CMD_RESP_LONG | CMD_WAIT_PRVDATA)?;

        // 8. CMD3: SEND_RELATIVE_ADDR (R6 response -> RCA in upper 16 bits).
        STATE.dbg_step = 8;
        let r6 = sdmmc_send_cmd(3, 0, CMD_RESP_EXPECT | CMD_CHECK_CRC | CMD_WAIT_PRVDATA)?;
        let rca = r6[0] & 0xffff_0000;
        if rca == 0 {
            return Err(());
        }

        // 9. CMD9(RCA): SEND_CSD (long 136-bit R2 response) -> decode total 512B sectors.
        STATE.dbg_step = 9;
        let r2_csd = sdmmc_send_cmd(9, rca, CMD_RESP_EXPECT | CMD_RESP_LONG | CMD_WAIT_PRVDATA)?;
        let sectors = parse_csd_sectors_r2(&r2_csd);
        if sectors == 0 {
            return Err(());
        }

        // 10. CMD7(RCA): SELECT_CARD -> enter Transfer state (`TRAN`).
        STATE.dbg_step = 10;
        let _ = sdmmc_send_cmd(7, rca, CMD_RESP_EXPECT | CMD_CHECK_CRC | CMD_WAIT_PRVDATA)?;
        if !sdmmc_wait_not_busy(500_000) {
            return Err(());
        }

        // 11. CMD16(512): SET_BLOCKLEN = 512 bytes.
        STATE.dbg_step = 11;
        let _ = sdmmc_send_cmd(
            16,
            SECTOR_BYTES as u32,
            CMD_RESP_EXPECT | CMD_CHECK_CRC | CMD_WAIT_PRVDATA,
        )?;

        // 12. CMD55(RCA) + ACMD42(0): SET_CLR_CARD_DETECT (disconnect internal 50K pull-up on D3).
        STATE.dbg_step = 12;
        if sdmmc_send_cmd(55, rca, CMD_RESP_EXPECT | CMD_CHECK_CRC | CMD_WAIT_PRVDATA).is_ok() {
            let _ = sdmmc_send_cmd(42, 0, CMD_RESP_EXPECT | CMD_CHECK_CRC | CMD_WAIT_PRVDATA);
        }

        // 13. CMD55(RCA) + ACMD6(2): SET_BUS_WIDTH to 4-bit SD bus (`D0..D3`), then enable
        //     4-bit mode (`CARD_WIDTH4 = 1`) in `SDHOST_CTYPE` and route `D3` (`GPIO42`) to IOMUX Func 0.
        STATE.dbg_step = 13;
        let _ = sdmmc_send_cmd(55, rca, CMD_RESP_EXPECT | CMD_CHECK_CRC | CMD_WAIT_PRVDATA)?;
        let _ = sdmmc_send_cmd(6, 2, CMD_RESP_EXPECT | CMD_CHECK_CRC | CMD_WAIT_PRVDATA)?;
        write(SDHOST_CTYPE, 1);
        configure_pin_sdmmc_iomux(PIN_D3, true);

        // 14. Switch to Default Speed 20 MHz (`160 MHz / 8 = 20 MHz`, `card_div = 0`) first.
        STATE.dbg_step = 14;
        let mut clk_khz = sdmmc_set_clock(8, 0)?;
        let mut high_speed = false;

        // 15. Attempt `CMD6` (`SWITCH_FUNC`, Mode 1 = switch Group 1 to Function 1 High-Speed).
        //     CMD6 returns a 64-byte status block over the 4-bit data bus via IDMAC.
        STATE.dbg_step = 15;
        let dma_bus = dma_buf_bus_addr();
        let dma_direct = dma_buf_direct_ptr();
        if sdmmc_transfer_idma(6, 0x80ff_fff1, dma_bus, 64, 64, false, false, service).is_ok() {
            let status_byte16 = dma_direct.add(16).read_volatile();
            if (status_byte16 & 0x0f) == 1 {
                delay_us(100);
                // Switch host clock to 40 MHz (`160 MHz / 4 = 40 MHz`, 20 MB/s 4-bit raw bus).
                if let Ok(hs_khz) = sdmmc_set_clock(4, 0) {
                    clk_khz = hs_khz;
                    high_speed = true;
                }
            }
        }

        STATE.dbg_step = 16;
        Ok((sdhc, high_speed, clk_khz, rca, ocr, sectors))
    }
}

// =============================================================================
// Legacy 1-bit SPI fallback (kept intact in case a card/adapter only supports SPI)
// =============================================================================

unsafe fn configure_pins_spi() {
    unsafe {
        for pin in [PIN_D0, PIN_D1, PIN_D2] {
            io::drive(pin, false);
            io::latch(pin, true);
            io::configure(pin, true);
        }
        io::latch(PIN_D3, true);
        io::configure(PIN_D3, true);
        io::drive(PIN_D3, true);

        io::latch(PIN_CMD, true);
        io::configure(PIN_CMD, true);
        io::drive(PIN_CMD, true);

        io::latch(PIN_CLK, false);
        io::configure(PIN_CLK, false);
        io::drive(PIN_CLK, true);
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

pub fn crc7(bytes: &[u8]) -> u8 {
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
    (crc & 0x7f) << 1 | 1
}

pub fn crc16_ccitt(bytes: &[u8]) -> u16 {
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

unsafe fn wait_not_busy_spi(slow: bool, max_bytes: usize) -> bool {
    unsafe {
        for _ in 0..max_bytes {
            if xfer(0xff, slow) == 0xff {
                return true;
            }
        }
    }
    false
}

unsafe fn send_cmd_spi(cmd: u8, arg: u32, slow: bool) -> Result<u8, ()> {
    let frame = [
        0x40 | (cmd & 0x3f),
        (arg >> 24) as u8,
        (arg >> 16) as u8,
        (arg >> 8) as u8,
        arg as u8,
    ];
    let crc = crc7(&frame);
    unsafe {
        if cmd != 0 && !wait_not_busy_spi(slow, 5000) {
            return Err(());
        }
        for &b in &frame {
            let _ = xfer(b, slow);
        }
        let _ = xfer(crc, slow);
        if cmd == 12 {
            let _ = xfer(0xff, slow);
        }
        for _ in 0..16 {
            let r1 = xfer(0xff, slow);
            if (r1 & 0x80) == 0 {
                return Ok(r1);
            }
        }
    }
    Err(())
}

pub fn parse_csd_sectors(csd: &[u8; 16]) -> u32 {
    let csd_structure = csd[0] >> 6;
    match csd_structure {
        0 => {
            let read_bl_len = u32::from(csd[5] & 0x0f);
            let c_size = (u32::from(csd[6] & 0x03) << 10)
                | (u32::from(csd[7]) << 2)
                | u32::from(csd[8] >> 6);
            let c_size_mult = (u32::from(csd[9] & 0x03) << 1) | u32::from(csd[10] >> 7);
            if !(9..=11).contains(&read_bl_len) {
                return 0;
            }
            let mult = 1u32 << (c_size_mult + 2);
            let blocknr = (c_size + 1).saturating_mul(mult);
            let block_len = 1u32 << read_bl_len;
            blocknr.saturating_mul(block_len) / 512
        }
        1 => {
            let c_size =
                (u32::from(csd[7] & 0x3f) << 16) | (u32::from(csd[8]) << 8) | u32::from(csd[9]);
            (c_size + 1).saturating_mul(1024)
        }
        _ => 0,
    }
}

unsafe fn read_csd_spi(slow: bool) -> Result<[u8; 16], ()> {
    unsafe {
        cs_select();
        let res = (|| {
            let r1 = send_cmd_spi(9, 0, slow)?;
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

unsafe fn try_init_spi(service: &mut impl FnMut()) -> Result<(bool, u32, u32), u32> {
    unsafe {
        power_ldo_vo4();
        configure_pins_spi();

        cs_deselect();
        for _ in 0..12 {
            let _ = xfer_slow(0xff);
        }

        let mut idle_ok = false;
        for _ in 0..8 {
            cs_select();
            let r1 = send_cmd_spi(0, 0, true);
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

        cs_select();
        let r1 = send_cmd_spi(8, 0x0000_01aa, true).map_err(|()| {
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

        cs_select();
        let r59 = send_cmd_spi(59, 1, true);
        cs_deselect();
        let _ = xfer_slow(0xff);
        if !matches!(r59, Ok(0x01 | 0x00)) {
            return Err(ERR_SD_INIT);
        }

        let acmd41_arg = if v2_card { 0x4000_0000 } else { 0 };
        let mut ready = false;
        for _ in 0..500 {
            service();
            cs_select();
            let r55 = send_cmd_spi(55, 0, true);
            cs_deselect();
            let _ = xfer_slow(0xff);
            if !matches!(r55, Ok(0x01 | 0x00)) {
                return Err(ERR_SD_INIT);
            }

            cs_select();
            let r41 = send_cmd_spi(41, acmd41_arg, true);
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

        cs_select();
        let r58 = send_cmd_spi(58, 0, false).map_err(|()| {
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

        if !sdhc {
            cs_select();
            let r16 = send_cmd_spi(16, SECTOR_BYTES as u32, false);
            cs_deselect();
            let _ = xfer_fast(0xff);
            if r16 != Ok(0x00) {
                return Err(ERR_SD_INIT);
            }
        }

        let csd = read_csd_spi(false).map_err(|()| ERR_SD_INIT)?;
        let sectors = parse_csd_sectors(&csd);
        if sectors == 0 {
            return Err(ERR_SD_INIT);
        }
        Ok((sdhc, ocr, sectors))
    }
}

/// Initialize the Tab5 microSD card (preferring the hardware 4-bit SDMMC Host Controller
/// at `0x5008_3000` with IDMAC, with automatic fallback to SPI mode).
///
/// # Safety
/// Exclusive HP0 ownership of `LDO_VO4`, `SDHOST`, and `GPIO39..44` with IRQs masked.
pub unsafe fn ensure_initialized() -> Result<(bool, bool, u32, u32), u32> {
    unsafe { ensure_initialized_with(&mut || {}) }
}

/// Same exclusive SD ownership as `ensure_initialized`. The callback may
/// service unrelated hardware while we wait, with kernel interrupts masked.
pub unsafe fn ensure_initialized_with(
    service: &mut impl FnMut(),
) -> Result<(bool, bool, u32, u32), u32> {
    unsafe {
        if STATE.ready {
            return Ok((false, STATE.sdhc, STATE.ocr, STATE.sectors));
        }
        if let Ok((sdhc, high_speed, clk_khz, rca, ocr, sectors)) = try_init_sdmmc_4bit(service) {
            STATE.ready = true;
            STATE.sdhc = sdhc;
            STATE.sdmmc_4bit = true;
            STATE.high_speed = high_speed;
            STATE.clk_khz = clk_khz;
            STATE.rca = rca;
            STATE.ocr = ocr;
            STATE.sectors = sectors;
            return Ok((true, sdhc, ocr, sectors));
        }
        match try_init_spi(service) {
            Ok((sdhc, ocr, sectors)) => {
                STATE.ready = true;
                STATE.sdhc = sdhc;
                STATE.sdmmc_4bit = false;
                STATE.high_speed = false;
                STATE.clk_khz = 1_500;
                STATE.rca = 0;
                STATE.ocr = ocr;
                STATE.sectors = sectors;
                Ok((true, sdhc, ocr, sectors))
            }
            Err(e) => {
                STATE.errors = STATE.errors.saturating_add(1);
                Err(e)
            }
        }
    }
}

// Only the current export session may read raw sectors while Linux owns the card.
static mut EXPORTED: bool = false;

/// Called exclusively with KERNEL_LOCK held.
pub unsafe fn set_exported(exported: bool) {
    unsafe { EXPORTED = exported };
}

pub unsafe fn exported() -> bool {
    unsafe { EXPORTED }
}

pub unsafe fn read_export_sector(
    lba: u32,
    out: &mut [u8; SECTOR_BYTES],
    service: &mut impl FnMut(),
) -> Result<(), u32> {
    unsafe {
        if !EXPORTED || !STATE.ready {
            return Err(ERR_SD_READ);
        }
        let ptr = read_sectors(lba, 1, service)?;
        core::ptr::copy_nonoverlapping(ptr, out.as_mut_ptr(), SECTOR_BYTES);
        Ok(())
    }
}

/// Commit one bounded USB batch. Large SDMMC writes use one CMD25 while
/// the CPU feeds the 32 KiB DMA ring from the kernel-owned PSRAM staging buffer.
pub unsafe fn write_export_sectors(
    lba: u32,
    data: &[u8],
    service: &mut impl FnMut(),
) -> Result<(), u32> {
    let count = data.len() / SECTOR_BYTES;
    if count == 0 || count > 256 || !data.len().is_multiple_of(SECTOR_BYTES) {
        return Err(ERR_SD_WRITE);
    }
    unsafe {
        if !EXPORTED
            || !STATE.ready
            || lba
                .checked_add(count as u32)
                .is_none_or(|end| end > STATE.sectors)
        {
            return Err(ERR_SD_WRITE);
        }
        if STATE.sdmmc_4bit && data.len() > BENCH_CHUNK_BYTES {
            return sdmmc_stream(lba, count as u32, true, service, |seg, offset, len| {
                core::ptr::copy_nonoverlapping(
                    data[offset..offset + len].as_ptr(),
                    dma_buf_direct_ptr().add(seg * SEG_BYTES),
                    len,
                );
                Ok(())
            });
        }
        for (i, chunk) in data.chunks(BENCH_CHUNK_BYTES).enumerate() {
            core::ptr::copy_nonoverlapping(chunk.as_ptr(), dma_buf_direct_ptr(), chunk.len());
            fence();
            write_sectors(
                lba + (i * BENCH_CHUNK_SECTORS) as u32,
                chunk.len() / SECTOR_BYTES,
                service,
            )?;
        }
        Ok(())
    }
}

/// The driver has no dirty write cache. Confirm that the current card is ready.
pub unsafe fn sync_export(service: &mut impl FnMut()) -> bool {
    unsafe {
        EXPORTED
            && STATE.ready
            && if STATE.sdmmc_4bit {
                sdmmc_wait_not_busy_with(4_000_000, service)
            } else {
                // SPI writes already include the card's busy response before deselect.
                true
            }
    }
}

/// Read `count` consecutive 512-byte sectors starting at `lba` (`1..=64` sectors)
/// directly into `DMA_BUF` using hardware IDMAC (`CMD17` for 1 sector, `CMD18` for multi-sector).
///
/// # Safety
/// Exclusive HP0 ownership of `SDHOST` and `DMA_BUF` with machine interrupts masked.
pub unsafe fn read_sectors_dma(lba: u32, count: usize) -> Result<*const u8, u32> {
    if unsafe { EXPORTED } {
        return Err(ERR_SD_READ);
    }
    unsafe { read_sectors_dma_with(lba, count, &mut || {}) }
}

/// Service unrelated hardware during DMA waits.
///
/// # Safety
/// Same ownership as read_sectors_dma. The callback must not access SD.
/// The returned buffer is valid only until the next SD operation.
pub unsafe fn read_sectors_dma_with(
    lba: u32,
    count: usize,
    service: &mut impl FnMut(),
) -> Result<*const u8, u32> {
    if unsafe { EXPORTED } {
        return Err(ERR_SD_READ);
    }
    unsafe { read_sectors(lba, count, service) }
}

unsafe fn read_sectors(
    lba: u32,
    count: usize,
    service: &mut impl FnMut(),
) -> Result<*const u8, u32> {
    if count == 0 || count > BENCH_CHUNK_SECTORS {
        return Err(ERR_SD_READ);
    }
    unsafe {
        if !STATE.ready {
            let _ = ensure_initialized_with(service)?;
        }
        if lba
            .checked_add(count as u32)
            .is_none_or(|end| end > STATE.sectors)
        {
            return Err(ERR_SD_READ);
        }
        let arg = if STATE.sdhc {
            lba
        } else {
            lba.checked_mul(SECTOR_BYTES as u32).ok_or(ERR_SD_READ)?
        };
        let dma_bus = dma_buf_bus_addr();
        let dma_ptr = dma_buf_direct_ptr();
        if STATE.sdmmc_4bit {
            let cmd = if count == 1 { 17 } else { 18 };
            STATE.read_commands = STATE.read_commands.saturating_add(1);
            let res = sdmmc_transfer_idma(
                cmd,
                arg,
                dma_bus,
                count * SECTOR_BYTES,
                SECTOR_BYTES,
                false,
                count > 1,
                service,
            );
            if res.is_ok() {
                STATE.read_sectors = STATE.read_sectors.saturating_add(count as u32);
                return Ok(dma_ptr as *const u8);
            }
            STATE.errors = STATE.errors.saturating_add(1);
            STATE.ready = false;
            return Err(ERR_SD_READ);
        }
        for i in 0..count {
            service();
            let slot = &mut *(dma_ptr.add(i * SECTOR_BYTES) as *mut [u8; SECTOR_BYTES]);
            read_sector_spi(lba + i as u32, slot)?;
        }
        Ok(dma_ptr as *const u8)
    }
}

unsafe fn write_sectors(lba: u32, count: usize, service: &mut impl FnMut()) -> Result<(), u32> {
    if count == 0 || count > BENCH_CHUNK_SECTORS {
        return Err(ERR_SD_WRITE);
    }
    unsafe {
        if !STATE.ready {
            let _ = ensure_initialized()?;
        }
        if lba
            .checked_add(count as u32)
            .is_none_or(|end| end > STATE.sectors)
        {
            return Err(ERR_SD_WRITE);
        }
        let arg = if STATE.sdhc {
            lba
        } else {
            lba.checked_mul(SECTOR_BYTES as u32).ok_or(ERR_SD_WRITE)?
        };
        let dma_bus = dma_buf_bus_addr();
        let dma_ptr = dma_buf_direct_ptr() as *const u8;
        if STATE.sdmmc_4bit {
            let cmd = if count == 1 { 24 } else { 25 };
            let res = sdmmc_transfer_idma(
                cmd,
                arg,
                dma_bus,
                count * SECTOR_BYTES,
                SECTOR_BYTES,
                true,
                count > 1,
                service,
            );
            if res.is_ok() {
                STATE.written_sectors = STATE.written_sectors.saturating_add(count as u32);
                return Ok(());
            }
            STATE.errors = STATE.errors.saturating_add(1);
            STATE.ready = false;
            return Err(ERR_SD_WRITE);
        }
        for i in 0..count {
            service();
            let slot = &*(dma_ptr.add(i * SECTOR_BYTES) as *const [u8; SECTOR_BYTES]);
            write_sector_spi(lba + i as u32, slot)?;
        }
        Ok(())
    }
}

unsafe fn read_sector_spi(lba: u32, out: &mut [u8; SECTOR_BYTES]) -> Result<(), u32> {
    unsafe { STATE.read_commands = STATE.read_commands.saturating_add(1) };
    unsafe {
        let arg = if STATE.sdhc {
            lba
        } else {
            lba.checked_mul(SECTOR_BYTES as u32).ok_or(ERR_SD_READ)?
        };
        cs_select();
        let res = (|| {
            let r1 = send_cmd_spi(17, arg, false).map_err(|()| ERR_SD_READ)?;
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

unsafe fn write_sector_spi(lba: u32, data: &[u8; SECTOR_BYTES]) -> Result<(), u32> {
    unsafe {
        let arg = if STATE.sdhc {
            lba
        } else {
            lba.checked_mul(SECTOR_BYTES as u32).ok_or(ERR_SD_WRITE)?
        };
        let crc = crc16_ccitt(data);
        cs_select();
        let res = (|| {
            let r1 = send_cmd_spi(24, arg, false).map_err(|()| ERR_SD_WRITE)?;
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
            if !wait_not_busy_spi(false, 250_000) {
                return Err(ERR_SD_WRITE);
            }
            let r13 = send_cmd_spi(13, 0, false).map_err(|()| ERR_SD_WRITE)?;
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

/// Read one 512-byte sector at `lba` using hardware 4-bit SDMMC IDMA (`CMD17`) or SPI fallback.
///
/// # Safety
/// Exclusive HP0 ownership of `SDHOST` / `GPIO39..44` with machine interrupts masked.
pub unsafe fn read_sector(lba: u32, out: &mut [u8; SECTOR_BYTES]) -> Result<(), u32> {
    unsafe { read_sector_with(lba, out, &mut || {}) }
}

/// Read one sector, allowing a callback to run during controller waits.
///
/// # Safety
/// Same ownership as `read_sector`. The callback must not access the SD driver.
pub unsafe fn read_sector_with(
    lba: u32,
    out: &mut [u8; SECTOR_BYTES],
    service: &mut impl FnMut(),
) -> Result<(), u32> {
    unsafe {
        if EXPORTED {
            return Err(ERR_SD_READ);
        }
        let ptr = read_sectors(lba, 1, service)?;
        core::ptr::copy_nonoverlapping(ptr, out.as_mut_ptr(), SECTOR_BYTES);
        Ok(())
    }
}

// Only primary MBR partitions are selected. A protective MBR blocks legacy
// writes across the disk even though the GPT is not yet mountable.
fn primary_partitions(mbr: &[u8; SECTOR_BYTES], sectors: u32) -> Result<[(u8, u32, u32); 4], u32> {
    let mut result = [(0, 0, 0); 4];
    if mbr[510..] != [0x55, 0xaa] {
        return Ok(result);
    }
    for (slot, entry) in result.iter_mut().zip(mbr[446..510].chunks_exact(16)) {
        let kind = entry[4];
        if kind == 0 {
            continue;
        }
        let start = u32::from_le_bytes(entry[8..12].try_into().unwrap());
        let count = u32::from_le_bytes(entry[12..16].try_into().unwrap());
        if start == 0 || count == 0 || start.checked_add(count).is_none_or(|end| end > sectors) {
            return Err(ERR_SD_READ);
        }
        *slot = (kind, start, count);
    }
    for (i, &(kind, start, count)) in result.iter().enumerate() {
        if kind != 0
            && result[..i]
                .iter()
                .any(|&(k, s, n)| k != 0 && start < s + n && s < start + count)
        {
            return Err(ERR_SD_READ);
        }
    }
    Ok(result)
}

/// Read the single Linux primary partition from the card's MBR.
/// # Safety
/// Same serialized controller ownership and callback restrictions as `read_sector_with`.
pub unsafe fn system_partition(service: &mut impl FnMut()) -> Result<(u32, u32), u32> {
    if unsafe { EXPORTED } {
        return Err(ERR_SD_READ);
    }
    let (_, _, _, sectors) = unsafe { ensure_initialized_with(service)? };
    let mut mbr = [0; SECTOR_BYTES];
    unsafe { read_sector_with(0, &mut mbr, service)? };
    let partitions = primary_partitions(&mbr, sectors)?;
    if partitions.iter().any(|&(kind, _, _)| kind == 0xee) {
        return Err(ERR_SD_READ);
    }
    let mut linux = partitions.into_iter().filter(|&(kind, _, _)| kind == 0x83);
    match (linux.next(), linux.next()) {
        (Some((_, start, count)), None) => Ok((start, count)),
        _ => Err(ERR_SD_READ),
    }
}

/// Check the entire legacy write range before issuing any data writes.
/// # Safety
/// Same serialized controller ownership as `read_sector`.
pub unsafe fn check_legacy_write_range(start: u32, count: u32) -> Result<(), u32> {
    let (_, _, _, sectors) = unsafe { ensure_initialized()? };
    let end = start.checked_add(count).ok_or(ERR_SD_WRITE)?;
    if start == 0 || count == 0 || end > sectors {
        return Err(ERR_SD_WRITE);
    }
    let mut buf = [0; SECTOR_BYTES];
    unsafe { read_sector(0, &mut buf)? };
    if buf[510..] == [0x55, 0xaa] {
        let partitions = primary_partitions(&buf, sectors)?;
        if partitions
            .iter()
            .any(|&(kind, s, n)| kind == 0xee || (kind != 0 && start < s + n && s < end))
        {
            return Err(ERR_SD_WRITE);
        }
        // A boot sector can also have 55aa without partition entries.
        if partitions.iter().all(|&(kind, _, _)| kind == 0) {
            return Err(ERR_SD_WRITE);
        }
    } else {
        unsafe { read_sector(2, &mut buf)? };
        if buf[0x38..0x3a] == [0x53, 0xef] {
            return Err(ERR_SD_WRITE);
        }
    }
    Ok(())
}

/// Write one 512-byte sector at `lba` using hardware 4-bit SDMMC IDMA (`CMD24`) or SPI fallback.
///
/// # Safety
/// Exclusive HP0 ownership of `SDHOST` / `GPIO39..44` with machine interrupts masked.
pub unsafe fn write_sector(lba: u32, data: &[u8; SECTOR_BYTES]) -> Result<(), u32> {
    unsafe { write_sector_with(lba, data, &mut || {}) }
}

/// Write one 512-byte sector at `lba`, allowing a keepalive callback to run during waits.
///
/// # Safety
/// Exclusive HP0 ownership of `SDHOST` / `GPIO39..44` with machine interrupts masked.
/// The callback must not access the SD driver.
pub unsafe fn write_sector_with(
    lba: u32,
    data: &[u8; SECTOR_BYTES],
    service: &mut impl FnMut(),
) -> Result<(), u32> {
    unsafe {
        if EXPORTED {
            return Err(ERR_SD_WRITE);
        }
        let dma_ptr = dma_buf_direct_ptr();
        core::ptr::copy_nonoverlapping(data.as_ptr(), dma_ptr, SECTOR_BYTES);
        fence();
        write_sectors(lba, 1, service)
    }
}

#[inline(always)]
unsafe fn desc_done(idx: usize) -> bool {
    unsafe { (idma_chain_direct_ptr(idx) as *const u32).read_volatile() & DES0_OWN == 0 }
}

/// One streaming `CMD25` (write) / `CMD18` (read) of `sectors` sectors at `lba`.
///
/// The descriptor chain walks the 32 KiB `DMA_BUF` as a ring of `NSEG` segments. While the IDMAC
/// transfers segment `k`, the CPU refills (write) / verifies (read) the segments it has already
/// finished. Write descriptors for later laps stay CPU-owned until their data is ready.
unsafe fn sdmmc_stream(
    lba: u32,
    sectors: u32,
    is_write: bool,
    keepalive: &mut impl FnMut(),
    mut segment: impl FnMut(usize, usize, usize) -> Result<(), u32>,
) -> Result<(), u32> {
    let err = if is_write { ERR_SD_WRITE } else { ERR_SD_READ };
    let total_bytes = sectors as usize * SECTOR_BYTES;
    let laps = total_bytes.div_ceil(SEG_BYTES);
    let ndesc = total_bytes.div_ceil(IDMA_DESC_MAX_BYTES);
    if laps == 0 || ndesc > IDMA_DESCS {
        return Err(err);
    }
    unsafe {
        let segment_len = |lap: usize| (total_bytes - lap * SEG_BYTES).min(SEG_BYTES);
        let buf = dma_buf_bus_addr();
        if !sdmmc_wait_not_busy_with(2_000_000, keepalive) {
            return Err(err);
        }
        if is_write {
            for seg in 0..NSEG.min(laps) {
                segment(seg, seg * SEG_BYTES, segment_len(seg))?;
            }
        }
        for i in 0..ndesc {
            let seg = (i / DESCS_PER_SEG) % NSEG;
            let addr = buf + seg * SEG_BYTES + (i % DESCS_PER_SEG) * IDMA_DESC_MAX_BYTES;
            let mut des0 = DES0_CH;
            if !is_write || i < NSEG * DESCS_PER_SEG {
                des0 |= DES0_OWN;
            }
            if i == 0 {
                des0 |= DES0_FD;
            }
            if i + 1 == ndesc {
                des0 |= DES0_LD;
            } else {
                des0 |= DES0_DIC;
            }
            idma_chain_direct_ptr(i).write_volatile(IdmaDesc {
                des0,
                des1: (total_bytes - i * IDMA_DESC_MAX_BYTES).min(IDMA_DESC_MAX_BYTES) as u32,
                des2: addr as u32,
                des3: if i + 1 == ndesc {
                    0
                } else {
                    idma_chain_bus_addr(i + 1) as u32
                },
            });
        }
        sdmmc_arm_idma_regs(total_bytes, SECTOR_BYTES);

        let arg = if STATE.sdhc {
            lba
        } else {
            lba.checked_mul(SECTOR_BYTES as u32).ok_or(err)?
        };
        let mut flags = CMD_RESP_EXPECT
            | CMD_CHECK_CRC
            | CMD_DATA_EXPECTED
            | CMD_WAIT_PRVDATA
            | CMD_USE_HOLD_REG
            | CMD_SEND_AUTO_STOP
            | CMD_START;
        if is_write {
            flags |= CMD_RW_WRITE;
        }
        write(SDHOST_CMDARG, arg);
        write(SDHOST_CMD, (if is_write { 25 } else { 18 }) | flags);

        let abort = |code: u32| -> Result<(), u32> {
            write(SDHOST_RINTSTS, 0xffff_ffff);
            let _ = sdmmc_send_cmd(12, 0, CMD_RESP_EXPECT | CMD_STOP_ABORT);
            STATE.errors = STATE.errors.saturating_add(1);
            STATE.ready = false;
            Err(code)
        };

        for lap in 0..laps {
            let last_desc = ((lap + 1) * DESCS_PER_SEG).min(ndesc) - 1;
            let mut spins = 0u32;
            while !desc_done(last_desc) {
                let rint = read(SDHOST_RINTSTS);
                if (rint & (CMD_ERR_MASK | DATA_ERR_MASK)) != 0 {
                    STATE.dbg_rint = rint;
                    return abort(err);
                }
                if is_write && read(SDHOST_IDSTS) & IDSTS_DESCRIPTOR_UNAVAILABLE != 0 {
                    write(SDHOST_IDSTS, IDSTS_DESCRIPTOR_UNAVAILABLE);
                    write(SDHOST_PLDMND, 1);
                }
                spins += 1;
                if spins & 0x3fff == 0 {
                    keepalive();
                }
                if spins > 60_000_000 {
                    STATE.dbg_rint = 0xeeee_0000 | rint;
                    return abort(err);
                }
            }
            let ahead = lap + NSEG;
            if is_write {
                if ahead < laps {
                    if let Err(code) = segment(lap % NSEG, ahead * SEG_BYTES, segment_len(ahead)) {
                        return abort(code);
                    }
                    fence();
                    let first = ahead * DESCS_PER_SEG;
                    let end = (first + DESCS_PER_SEG).min(ndesc);
                    // Publish the first descriptor last. A controller that caught
                    // up can resume only after the whole segment is visible.
                    for idx in (first + 1..end).rev() {
                        let flags = idma_chain_direct_ptr(idx).cast::<u32>();
                        flags.write_volatile(flags.read_volatile() | DES0_OWN);
                    }
                    fence();
                    let flags = idma_chain_direct_ptr(first).cast::<u32>();
                    flags.write_volatile(flags.read_volatile() | DES0_OWN);
                    fence();
                    write(SDHOST_PLDMND, 1);
                }
            } else {
                fence();
                if let Err(code) = segment(lap % NSEG, lap * SEG_BYTES, segment_len(lap)) {
                    return abort(code);
                }
                let guard = ahead * DESCS_PER_SEG - 1;
                if guard < ndesc && desc_done(guard) {
                    return abort(ERR_SD_OVERRUN);
                }
            }
            if (lap & 31) == 31 {
                keepalive();
            }
        }

        let wanted = INT_CMD_DONE | INT_DTO | INT_ACD;
        let mut seen = 0u32;
        for i in 0..8_000_000 {
            if i & 0x3fff == 0 {
                keepalive();
            }
            let rint = read(SDHOST_RINTSTS);
            if (rint & (CMD_ERR_MASK | DATA_ERR_MASK)) != 0 {
                STATE.dbg_rint = rint;
                return abort(err);
            }
            seen |= rint;
            if (seen & wanted) == wanted {
                break;
            }
        }
        if (seen & wanted) != wanted {
            STATE.dbg_rint = seen;
            return abort(err);
        }
        let response = read(SDHOST_RESP0);
        if response & 0xfdf9_0000 != 0 {
            STATE.dbg_rint = response;
            return abort(err);
        }
        write(SDHOST_RINTSTS, 0xffff_ffff);
        write(SDHOST_IDSTS, 0xffff_ffff);
        fence();
        if is_write && !sdmmc_wait_not_busy_with(40_000_000, keepalive) {
            return abort(err);
        }
        if is_write {
            STATE.written_sectors = STATE.written_sectors.saturating_add(sectors);
        } else {
            STATE.read_sectors = STATE.read_sectors.saturating_add(sectors);
        }
        Ok(())
    }
}
