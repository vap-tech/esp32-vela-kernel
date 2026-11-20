//! microSD-backed RGB565 assets, counterclockwise for keyboard-down landscape.
//!
//! At runtime, display assets (`DIAGNOSTIC` and `BOOT`) are streamed in 4 KiB
//! (8-sector) chunks from the Tab5 microSD card starting at `SD_ASSET_LBA` (`LBA 2048`),
//! with full `VELAIMG1` header validation, pixel-count bounds checking, `CRC32`
//! verification, and PSRAM framebuffer readback verification.
//! If a blank or unprovisioned microSD card is inserted, the 271,920-byte `VELAIMG1`
//! bundle is staged once from the Flash asset partition (`0x100000`) onto the
//! microSD card at `SD_ASSET_LBA` and verified back from microSD before decoding.
use crate::soc::{
    flash, psram,
    sdcard::{self, Probe as SdProbe, SECTOR_BYTES},
};

pub struct Asset {
    address: u32,
    length: usize,
    crc: u32,
}
include!(concat!(env!("OUT_DIR"), "/display_assets.rs"));

pub const FLASH_BUNDLE_BASE: u32 = 0x100000;
pub const SD_ASSET_LBA: u32 = 2048; // 1 MiB offset on microSD card

static mut SD_BUNDLE_VERIFIED: bool = false;
static mut SD_BUNDLE_PROVISIONED: bool = false;

#[derive(Clone, Copy)]
pub struct AssetStatus {
    pub verified: bool,
    pub provisioned: bool,
    pub asset_lba: u32,
    pub asset_bytes: u32,
}

#[repr(align(4))]
struct AlignedChunk([u32; 1024]);

static mut READ_CHUNK: AlignedChunk = AlignedChunk([0; 1024]);
static mut CACHED_ASSET_ADDR: u32 = 0;
static mut CACHED_BASE: usize = 0;
static mut CACHED_LOADED: usize = 0;
static mut STREAM_ASSET_ADDR: u32 = 0;
static mut STREAM_CURSOR: usize = 0;
static mut STREAM_PIXEL_POS: usize = 0;
static mut STREAM_RUN_REM: usize = 0;
static mut STREAM_RUN_PIXEL: u16 = 0;

unsafe fn reset_stream_cache() {
    unsafe {
        CACHED_ASSET_ADDR = 0;
        CACHED_BASE = 0;
        CACHED_LOADED = 0;
        STREAM_ASSET_ADDR = 0;
        STREAM_CURSOR = 0;
        STREAM_PIXEL_POS = 0;
        STREAM_RUN_REM = 0;
        STREAM_RUN_PIXEL = 0;
    }
}

/// Return current microSD asset verification status.
///
/// # Safety
/// Called on HP0 with machine interrupts masked.
pub unsafe fn status() -> AssetStatus {
    unsafe {
        AssetStatus {
            verified: SD_BUNDLE_VERIFIED,
            provisioned: SD_BUNDLE_PROVISIONED,
            asset_lba: SD_ASSET_LBA,
            asset_bytes: bundle_total_bytes().unwrap_or(0) as u32,
        }
    }
}

/// Probe the Tab5 microSD card and ensure the `VELAIMG1` asset bundle at `SD_ASSET_LBA`
/// is provisioned and verified (`CRC32` + pixel counts for both streams).
///
/// # Safety
/// Exclusive HP0 ownership of microSD SPI (`GPIO39..44`) and Flash staging ROM reads
/// with machine interrupts masked.
pub unsafe fn probe_and_verify() -> Result<SdProbe, u32> {
    unsafe {
        let (_, sdhc, ocr, sectors) = sdcard::ensure_initialized()?;
        let _ = ensure_sd_asset_ready(&DIAGNOSTIC)?;
        verify_stream_on_sd(&BOOT)?;
        Ok(SdProbe {
            sdhc,
            ocr,
            sectors,
            asset_lba: SD_ASSET_LBA,
            provisioned: SD_BUNDLE_PROVISIONED,
        })
    }
}

unsafe fn read_sd_words(byte_offset: u32, out: &mut [u32]) -> Result<(), u32> {
    if byte_offset as usize % 4 != 0 {
        return Err(13);
    }
    let mut byte_pos = byte_offset as usize;
    let total_bytes = out.len() * 4;
    let end_pos = byte_pos.checked_add(total_bytes).ok_or(13u32)?;
    let mut sector_buf = [0u8; SECTOR_BYTES];
    let mut out_idx = 0usize;

    while byte_pos < end_pos {
        let sector_idx = (byte_pos / SECTOR_BYTES) as u32;
        let lba = SD_ASSET_LBA
            .checked_add(sector_idx)
            .ok_or(sdcard::ERR_SD_READ)?;
        unsafe {
            sdcard::read_sector(lba, &mut sector_buf)?;
        }
        let in_sector = byte_pos % SECTOR_BYTES;
        let avail = (SECTOR_BYTES - in_sector).min(end_pos - byte_pos);
        for chunk in sector_buf[in_sector..in_sector + avail].chunks_exact(4) {
            out[out_idx] = u32::from_le_bytes(chunk.try_into().unwrap());
            out_idx += 1;
        }
        byte_pos += avail;
    }
    Ok(())
}

unsafe fn sd_header_matches() -> Result<bool, u32> {
    let mut header = [0u32; 16];
    unsafe { read_sd_words(0, &mut header)? };
    for (word, expected) in header.iter().zip(HEADER.chunks_exact(4)) {
        if *word != u32::from_le_bytes(expected.try_into().unwrap()) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn bundle_total_bytes() -> Result<usize, u32> {
    let mut end = HEADER.len();
    for asset in [&DIAGNOSTIC, &BOOT] {
        let offset = (asset.address as usize)
            .checked_sub(FLASH_BUNDLE_BASE as usize)
            .ok_or(13u32)?;
        let asset_end = offset.checked_add(asset.length).ok_or(13u32)?;
        if asset_end > end {
            end = asset_end;
        }
    }
    Ok(end)
}

unsafe fn provision_sd_from_flash() -> Result<(), u32> {
    // Validate the Flash staging copy header first before writing to microSD.
    let mut flash_header = [0u32; 16];
    unsafe { flash::read(FLASH_BUNDLE_BASE, &mut flash_header)? };
    for (word, expected) in flash_header.iter().zip(HEADER.chunks_exact(4)) {
        if *word != u32::from_le_bytes(expected.try_into().unwrap()) {
            return Err(17);
        }
    }

    let total_bytes = bundle_total_bytes()?;
    let total_sectors = (total_bytes + SECTOR_BYTES - 1) / SECTOR_BYTES;
    let mut words = [0u32; SECTOR_BYTES / 4];
    let mut sector_bytes = [0u8; SECTOR_BYTES];

    for sector in 0..total_sectors {
        let byte_off = sector * SECTOR_BYTES;
        let valid_bytes = (total_bytes - byte_off).min(SECTOR_BYTES);
        let valid_words = (valid_bytes + 3) / 4;
        words.fill(0);
        unsafe {
            flash::read(
                FLASH_BUNDLE_BASE + byte_off as u32,
                &mut words[..valid_words],
            )?;
        }
        for (i, &w) in words.iter().enumerate() {
            sector_bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        if valid_bytes < SECTOR_BYTES {
            sector_bytes[valid_bytes..].fill(0);
        }
        let lba = SD_ASSET_LBA
            .checked_add(sector as u32)
            .ok_or(sdcard::ERR_SD_WRITE)?;
        unsafe {
            sdcard::write_sector(lba, &sector_bytes)?;
        }
    }
    Ok(())
}

/// Fetch one 32-bit RLE word at `cursor` within `asset` using the shared 4 KiB
/// (`READ_CHUNK`, 8 × 512-byte sectors) microSD read buffer in BSS.
unsafe fn fetch_rle_word(asset: &'static Asset, cursor: usize) -> Result<Option<u32>, u32> {
    if cursor == asset.length {
        return Ok(None);
    }
    unsafe {
        if CACHED_ASSET_ADDR != asset.address
            || CACHED_LOADED == 0
            || cursor < CACHED_BASE
            || cursor >= CACHED_BASE + CACHED_LOADED
        {
            let loaded = (asset.length - cursor).min(4096);
            let rel_offset = asset
                .address
                .checked_sub(FLASH_BUNDLE_BASE)
                .and_then(|off| off.checked_add(cursor as u32))
                .ok_or(13u32)?;
            read_sd_words(rel_offset, &mut READ_CHUNK.0[..loaded / 4])?;
            CACHED_ASSET_ADDR = asset.address;
            CACHED_BASE = cursor;
            CACHED_LOADED = loaded;
        }
        Ok(Some(READ_CHUNK.0[(cursor - CACHED_BASE) / 4]))
    }
}

unsafe fn verify_stream_on_sd(asset: &'static Asset) -> Result<(), u32> {
    if asset.length == 0 || asset.length % 4 != 0 {
        return Err(13);
    }
    let mut cursor = 0usize;
    let mut crc = u32::MAX;
    let mut pixels = 0usize;
    while let Some(word) = unsafe { fetch_rle_word(asset, cursor)? } {
        cursor += 4;
        let count = (word & 0xffff) as usize;
        if count == 0 {
            return Err(13);
        }
        let pixel = (word >> 16) as u16;
        pixels = pixels
            .checked_add(count)
            .filter(|&n| n <= psram::FRAME_BYTES / 2)
            .ok_or(13u32)?;
        for byte in (count as u16)
            .to_le_bytes()
            .into_iter()
            .chain(pixel.to_le_bytes())
        {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
            }
        }
    }
    if pixels != psram::FRAME_BYTES / 2 {
        return Err(13);
    }
    if !crc != asset.crc {
        return Err(18);
    }
    Ok(())
}

unsafe fn ensure_sd_asset_ready(asset: &'static Asset) -> Result<Option<SdProbe>, u32> {
    unsafe {
        let (first_init, sdhc, ocr, sectors) = sdcard::ensure_initialized()?;
        let total_sectors = ((bundle_total_bytes()? + SECTOR_BYTES - 1) / SECTOR_BYTES) as u32;
        if SD_ASSET_LBA
            .checked_add(total_sectors)
            .filter(|&end| end <= sectors)
            .is_none()
        {
            return Err(sdcard::ERR_SD_INIT);
        }

        let mut provisioned = false;
        if !SD_BUNDLE_VERIFIED {
            reset_stream_cache();
            if !sd_header_matches()?
                || verify_stream_on_sd(&DIAGNOSTIC).is_err()
                || verify_stream_on_sd(&BOOT).is_err()
            {
                provision_sd_from_flash()?;
                provisioned = true;
                SD_BUNDLE_PROVISIONED = true;
                reset_stream_cache();
                if !sd_header_matches()? {
                    return Err(17);
                }
                verify_stream_on_sd(&DIAGNOSTIC)?;
                verify_stream_on_sd(&BOOT)?;
            }
            SD_BUNDLE_VERIFIED = true;
        } else {
            if !sd_header_matches()? {
                SD_BUNDLE_VERIFIED = false;
                return Err(17);
            }
            verify_stream_on_sd(asset)?;
        }

        let probe = if first_init || provisioned {
            Some(SdProbe {
                sdhc,
                ocr,
                sectors,
                asset_lba: SD_ASSET_LBA,
                provisioned: SD_BUNDLE_PROVISIONED,
            })
        } else {
            None
        };
        Ok(probe)
    }
}

/// Fill and verify one bounded row range in the dynamically allocated PSRAM
/// framebuffer at direct uncached address `frame_direct_base` by streaming
/// 4 KiB RLE chunks sequentially from the Tab5 microSD card.
///
/// # Safety
/// Exclusive HP0 ownership of microSD SPI (`GPIO39..44`), Flash staging ROM reads,
/// and the mapped PSRAM framebuffer window (`frame_direct_base..+FRAME_BYTES`) with IRQs masked.
pub unsafe fn draw_rows(
    asset: &'static Asset,
    frame_direct_base: usize,
    row: usize,
    rows: usize,
) -> (Option<SdProbe>, Result<(), u32>) {
    if frame_direct_base < psram::POOL_START
        || frame_direct_base + psram::FRAME_BYTES > psram::POOL_END
    {
        return (None, Err(12));
    }
    let Some(first) = row.checked_mul(720) else {
        return (None, Err(13));
    };
    let Some(end) = rows
        .checked_mul(720)
        .and_then(|n| first.checked_add(n))
        .filter(|&n| n <= psram::FRAME_BYTES / 2)
    else {
        return (None, Err(13));
    };
    let mut sd_probe = None;
    unsafe {
        if row == 0 {
            match ensure_sd_asset_ready(asset) {
                Ok(probe) => sd_probe = probe,
                Err(code) => return (None, Err(code)),
            }
            STREAM_ASSET_ADDR = asset.address;
            STREAM_CURSOR = 0;
            STREAM_PIXEL_POS = 0;
            STREAM_RUN_REM = 0;
            STREAM_RUN_PIXEL = 0;
        } else if STREAM_ASSET_ADDR != asset.address || STREAM_PIXEL_POS != first {
            STREAM_ASSET_ADDR = asset.address;
            STREAM_CURSOR = 0;
            STREAM_PIXEL_POS = 0;
            STREAM_RUN_REM = 0;
            STREAM_RUN_PIXEL = 0;
        }

        while STREAM_PIXEL_POS < end {
            if STREAM_RUN_REM == 0 {
                let word = match fetch_rle_word(asset, STREAM_CURSOR) {
                    Ok(Some(w)) => w,
                    Ok(None) => return (sd_probe, Err(13)),
                    Err(code) => return (sd_probe, Err(code)),
                };
                STREAM_CURSOR += 4;
                let count = (word & 0xffff) as usize;
                if count == 0 {
                    return (sd_probe, Err(13));
                }
                STREAM_RUN_REM = count;
                STREAM_RUN_PIXEL = (word >> 16) as u16;
            }
            let pos = STREAM_PIXEL_POS;
            let Some(run_end) = pos
                .checked_add(STREAM_RUN_REM)
                .filter(|&n| n <= psram::FRAME_BYTES / 2)
            else {
                return (sd_probe, Err(13));
            };
            let step_end = run_end.min(end);
            let consumed = step_end - pos;
            let pixel = STREAM_RUN_PIXEL;
            if step_end > first {
                let draw_start = pos.max(first);
                for index in draw_start..step_end {
                    let address = (frame_direct_base + index * 2) as *mut u16;
                    address.write_volatile(pixel);
                }
                for index in draw_start..step_end {
                    let address = (frame_direct_base + index * 2) as *const u16;
                    if address.read_volatile() != pixel {
                        return (sd_probe, Err(14));
                    }
                }
            }
            STREAM_PIXEL_POS = step_end;
            STREAM_RUN_REM -= consumed;
        }
    }
    (sd_probe, Ok(()))
}
