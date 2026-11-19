//! Flash-backed RGB565 assets, counterclockwise for keyboard-down landscape.
use crate::soc::{flash, psram};
pub struct Asset {
    address: u32,
    length: usize,
    crc: u32,
}
include!(concat!(env!("OUT_DIR"), "/display_assets.rs"));

/// One aligned 4 KiB Flash read buffer, reused for both decoding passes.
struct Reader {
    asset: &'static Asset,
    cursor: usize,
    base: usize,
    loaded: usize,
    words: [u32; 1024],
}
impl Reader {
    fn new(asset: &'static Asset) -> Self {
        Self {
            asset,
            cursor: 0,
            base: 0,
            loaded: 0,
            words: [0; 1024],
        }
    }
    fn rewind(&mut self) {
        self.cursor = 0;
        self.loaded = 0;
    }
    fn next(&mut self) -> Result<Option<(usize, u16)>, u32> {
        if self.cursor == self.asset.length {
            return Ok(None);
        }
        if self.cursor >= self.base + self.loaded || self.loaded == 0 {
            self.base = self.cursor;
            self.loaded = (self.asset.length - self.cursor).min(4096);
            flash::read(
                self.asset.address + self.cursor as u32,
                &mut self.words[..self.loaded / 4],
            )?;
        }
        let word = self.words[(self.cursor - self.base) / 4];
        self.cursor += 4;
        let count = (word & 0xffff) as usize;
        if count == 0 {
            return Err(13);
        }
        Ok(Some((count, (word >> 16) as u16)))
    }
}

fn verify(asset: &'static Asset) -> Result<(), u32> {
    let mut header = [0u32; 16];
    flash::read(0x100000, &mut header)?;
    for (word, expected) in header.iter().zip(HEADER.chunks_exact(4)) {
        if *word != u32::from_le_bytes(expected.try_into().unwrap()) {
            return Err(17);
        }
    }
    if asset.length == 0 || asset.length % 4 != 0 {
        return Err(13);
    }
    let mut reader = Reader::new(asset);
    let mut crc = u32::MAX;
    let mut pixels = 0usize;
    while let Some((count, pixel)) = reader.next()? {
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

/// Fill and verify one bounded row range in the kernel-owned framebuffer.
pub fn draw_rows(asset: &'static Asset, row: usize, rows: usize) -> Result<(), u32> {
    let first = row.checked_mul(720).ok_or(13u32)?;
    let end = rows
        .checked_mul(720)
        .and_then(|n| first.checked_add(n))
        .filter(|&n| n <= psram::FRAME_BYTES / 2)
        .ok_or(13u32)?;
    if row == 0 {
        verify(asset)?;
    }
    let mut reader = Reader::new(asset);
    // The second pass compares CPU readback against the same Flash source.
    for readback in [false, true] {
        reader.rewind();
        let mut position = 0usize;
        while position < end {
            let (count, pixel) = reader.next()?.ok_or(13u32)?;
            let next = position
                .checked_add(count)
                .filter(|&n| n <= psram::FRAME_BYTES / 2)
                .ok_or(13u32)?;
            for index in position.max(first)..next.min(end) {
                let address = (psram::DIRECT + index * 2) as *mut u16;
                unsafe {
                    if readback {
                        if address.read_volatile() != pixel {
                            return Err(14);
                        }
                    } else {
                        address.write_volatile(pixel);
                    }
                }
            }
            position = next;
        }
    }
    Ok(())
}
