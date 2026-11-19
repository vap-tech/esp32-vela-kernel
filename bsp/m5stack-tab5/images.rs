//! PNG-derived RGB565 assets, rotated clockwise for portrait scanout.
pub static DIAGNOSTIC: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vela_display_test.rle"));
pub static BOOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vela_boot.rle"));

/// Fill and verify a bounded row range in the kernel-owned framebuffer.
/// The run stream stores little-endian (count:u16, RGB565:u16) pairs.
pub fn draw_rows(stream: &[u8], row: usize, rows: usize) -> Result<(), u32> {
    let first = row.checked_mul(720).ok_or(13u32)?;
    let end = first
        .checked_add(rows * 720)
        .filter(|&n| n <= crate::soc::psram::FRAME_BYTES / 2)
        .ok_or(13u32)?;
    let mut position = 0usize;
    for token in stream.chunks_exact(4) {
        let count = usize::from(u16::from_le_bytes([token[0], token[1]]));
        let pixel = u16::from_le_bytes([token[2], token[3]]);
        let next = position
            .checked_add(count)
            .filter(|&n| n <= crate::soc::psram::FRAME_BYTES / 2)
            .ok_or(13u32)?;
        if count == 0 {
            return Err(13);
        }
        for index in position.max(first)..next.min(end) {
            let address = (crate::soc::psram::DIRECT + index * 2) as *mut u16;
            unsafe {
                address.write_volatile(pixel);
            }
        }
        position = next;
        if position >= end {
            break;
        }
    }
    if position < end {
        return Err(13);
    }
    let mut position = 0usize;
    for token in stream.chunks_exact(4) {
        let count = usize::from(u16::from_le_bytes([token[0], token[1]]));
        let pixel = u16::from_le_bytes([token[2], token[3]]);
        let next = position + count;
        for index in position.max(first)..next.min(end) {
            if unsafe { ((crate::soc::psram::DIRECT + index * 2) as *const u16).read_volatile() }
                != pixel
            {
                return Err(14);
            }
        }
        position = next;
        if position >= end {
            break;
        }
    }
    Ok(())
}
