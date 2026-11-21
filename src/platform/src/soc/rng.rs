//! Hardware True Random Number Generator (`TRNG`) for `ESP32-P4 v1.3`.
//!
//! Hardware register layout (`LP_SYSTEM` at `0x5011_0000`, `lp_system_reg.h`):
//! - `LP_SYSTEM_REG_RNG_DATA_REG` (`0x5011_01a4`): 32-bit hardware entropy word (`WDEV_RND_REG`).
//! - `LP_SYSTEM_REG_RNG_CFG_REG` (`0x5011_01c0`):
//!   - bit 0: `LP_SYSTEM_REG_RNG_TIMER_EN` (`1` = enable high-speed RNG timer)
//!   - bits `[8:1]`: `LP_SYSTEM_REG_RNG_TIMER_PSCALE` (`1` = fastest prescaler)
//!   - bit 9: `LP_SYSTEM_REG_RNG_SAR_ENABLE` (`1` = enable SAR ADC entropy source)
//!
//! Each call mixes fresh hardware TRNG samples from `0x5011_01a4`, the 64-bit `mcycle`
//! hardware counter, the 16 MHz `SYSTIMER` counter, and the LP slow-clock counter
//! through a SplitMix64 avalanche mixer so both single-word queries (`/dev/random`)
//! and high-rate bulk fills (`/dev/urandom`, SDMMC DMA benchmark buffers) carry
//! non-deterministic hardware entropy with full bit diffusion.
use crate::{
    arch,
    pac::{read, write},
    soc::{lp_counter, timer},
};

const LP_SYS_BASE: usize = 0x5011_0000;
const RNG_DATA_REG: usize = LP_SYS_BASE + 0x01a4;
const RNG_CFG_REG: usize = LP_SYS_BASE + 0x01c0;

const RNG_TIMER_EN: u32 = 1 << 0;
const RNG_TIMER_PSCALE_SHIFT: u32 = 1;
const RNG_TIMER_PSCALE_MASK: u32 = 0xff << RNG_TIMER_PSCALE_SHIFT;
const RNG_SAR_ENABLE: u32 = 1 << 9;

struct TrngState {
    initialized: bool,
    state: u64,
    samples: u64,
    last_raw: u32,
}

static mut TRNG: TrngState = TrngState {
    initialized: false,
    state: 0x9e37_79b9_7f4a_7c15,
    samples: 0,
    last_raw: 0,
};

#[inline(always)]
fn splitmix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Enable the `ESP32-P4` hardware TRNG timer and SAR entropy source in `LP_SYSTEM`.
///
/// # Safety
/// Serialized by `KERNEL_LOCK` with machine interrupts masked.
pub unsafe fn ensure_initialized() {
    unsafe {
        if TRNG.initialized {
            return;
        }
        let cfg = read(RNG_CFG_REG);
        write(
            RNG_CFG_REG,
            (cfg & !RNG_TIMER_PSCALE_MASK)
                | RNG_TIMER_EN
                | (1 << RNG_TIMER_PSCALE_SHIFT)
                | RNG_SAR_ENABLE,
        );
        let raw0 = read(RNG_DATA_REG);
        let cyc = arch::mcycle();
        let sys16 = timer::counter_16mhz().unwrap_or(0);
        let lp = lp_counter::now();
        let raw1 = read(RNG_DATA_REG);
        let seed = ((u64::from(raw0) << 32) | u64::from(raw1))
            ^ cyc.rotate_left(17)
            ^ sys16.rotate_left(37)
            ^ lp.rotate_left(51);
        TRNG.state = splitmix64(TRNG.state ^ seed);
        TRNG.last_raw = raw1;
        TRNG.samples = TRNG.samples.saturating_add(2);
        TRNG.initialized = true;
    }
}

/// Sample one conditioned 64-bit random word backed by live `LP_SYSTEM_REG_RNG_DATA_REG`
/// hardware entropy and cycle/timer jitter.
///
/// # Safety
/// Serialized by `KERNEL_LOCK` with machine interrupts masked.
pub unsafe fn next_u64() -> u64 {
    unsafe {
        ensure_initialized();
        let raw_a = read(RNG_DATA_REG);
        let cyc = arch::mcycle();
        let lp = lp_counter::now();
        let raw_b = read(RNG_DATA_REG);
        let hw = ((u64::from(raw_a) << 32) | u64::from(raw_b.rotate_left(13)))
            ^ cyc
            ^ lp.rotate_left(29);
        TRNG.last_raw = raw_b;
        TRNG.samples = TRNG.samples.saturating_add(2);
        TRNG.state = TRNG.state.wrapping_add(0x9e37_79b9_7f4a_7c15 ^ hw);
        splitmix64(TRNG.state ^ hw.rotate_left(23))
    }
}

/// Sample one conditioned 32-bit random word from the hardware TRNG.
///
/// # Safety
/// Serialized by `KERNEL_LOCK` with machine interrupts masked.
pub unsafe fn next_u32() -> u32 {
    unsafe {
        let w = next_u64();
        (w as u32) ^ ((w >> 32) as u32)
    }
}

/// Return `(cfg_reg, last_raw_sample, total_hw_samples, random_words_8)` for `STATS_RANDOM`.
///
/// # Safety
/// Serialized by `KERNEL_LOCK` with machine interrupts masked.
pub unsafe fn snapshot() -> [u32; 12] {
    unsafe {
        ensure_initialized();
        let mut words = [0u32; 8];
        for slot in &mut words {
            *slot = next_u32();
        }
        let cfg = read(RNG_CFG_REG) & 0x3ff;
        let samples = TRNG.samples;
        [
            cfg,
            TRNG.last_raw,
            samples as u32,
            (samples >> 32) as u32,
            words[0],
            words[1],
            words[2],
            words[3],
            words[4],
            words[5],
            words[6],
            words[7],
        ]
    }
}
