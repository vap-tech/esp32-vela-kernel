//! Pure SD / SDHC / SDXC protocol helpers (`CRC7`, `CRC16-CCITT`, CSD v1/v2 capacity
//! decoding, and `VELAIMG1` asset LBA bounds checking).

pub const SECTOR_BYTES: usize = 512;
pub const SD_ASSET_LBA: u32 = 2048; // 1 MiB offset on microSD card

/// Compute the 7-bit SD command CRC with trailing end bit (`(crc7 << 1) | 1`).
pub const fn crc7(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    let mut i = 0;
    while i < bytes.len() {
        let mut data = bytes[i];
        let mut bit = 0;
        while bit < 8 {
            crc <<= 1;
            if ((data ^ crc) & 0x80) != 0 {
                crc ^= 0x09;
            }
            data <<= 1;
            bit += 1;
        }
        i += 1;
    }
    (crc << 1) | 1
}

/// Compute the 16-bit CCITT CRC (`0x1021`, initial `0x0000`) for SD SPI data blocks.
pub const fn crc16_ccitt(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    let mut i = 0;
    while i < bytes.len() {
        crc ^= (bytes[i] as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            if (crc & 0x8000) != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
            bit += 1;
        }
        i += 1;
    }
    crc
}

/// Extract total 512-byte sector count from a 16-byte CSD v1.0 (SDSC) or v2.0 (SDHC/SDXC) register.
pub const fn parse_csd_sectors(csd: &[u8; 16]) -> u32 {
    let structure = csd[0] >> 6;
    match structure {
        // CSD Version 2.0 (SDHC / SDXC): C_SIZE is bits [69:48] -> (C_SIZE + 1) * 1024 sectors.
        1 => {
            let c_size = ((csd[7] & 0x3f) as u32) << 16 | (csd[8] as u32) << 8 | (csd[9] as u32);
            (c_size + 1).saturating_mul(1024)
        }
        // CSD Version 1.0 (SDSC): C_SIZE [73:62], C_SIZE_MULT [49:47], READ_BL_LEN [83:80].
        0 => {
            let read_bl_len = (csd[5] & 0x0f) as u32;
            if read_bl_len < 9 || read_bl_len > 11 {
                return 0;
            }
            let c_size =
                ((csd[6] & 0x03) as u32) << 10 | (csd[7] as u32) << 2 | ((csd[8] >> 6) as u32);
            let c_size_mult = ((csd[9] & 0x03) as u32) << 1 | ((csd[10] >> 7) as u32);
            let blocknr = (c_size + 1) << (c_size_mult + 2);
            let block_len = 1u32 << read_bl_len;
            (blocknr.saturating_mul(block_len)) / 512
        }
        _ => 0,
    }
}

/// Check whether an asset bundle of `total_bytes` fits on a card of `card_sectors`
/// starting at `asset_lba`.
pub const fn asset_span_fits(asset_lba: u32, total_bytes: usize, card_sectors: u32) -> bool {
    if total_bytes == 0 || card_sectors == 0 {
        return false;
    }
    let sectors_needed = ((total_bytes + SECTOR_BYTES - 1) / SECTOR_BYTES) as u32;
    match asset_lba.checked_add(sectors_needed) {
        Some(end) => end <= card_sectors,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc7_matches_sd_spec_vectors() {
        // CMD0 (0x40, 0, 0, 0, 0) -> 0x95
        assert_eq!(crc7(&[0x40, 0x00, 0x00, 0x00, 0x00]), 0x95);
        // CMD8 (0x48, 0, 0, 1, 0xaa) -> 0x87
        assert_eq!(crc7(&[0x48, 0x00, 0x00, 0x01, 0xaa]), 0x87);
        // CMD17 (0x51, 0, 0, 0, 0) -> 0x55
        assert_eq!(crc7(&[0x51, 0x00, 0x00, 0x00, 0x00]), 0x55);
    }

    #[test]
    fn crc16_ccitt_matches_standard_vector_and_512b_block() {
        // Standard CCITT-FALSE ("123456789" with init 0x0000 -> 0x31c3)
        assert_eq!(crc16_ccitt(b"123456789"), 0x31c3);
        // 512 bytes of 0xff -> 0x7fa1 per SD Physical Layer Specification
        let ff_block = [0xffu8; SECTOR_BYTES];
        assert_eq!(crc16_ccitt(&ff_block), 0x7fa1);
    }

    #[test]
    fn parses_sdhc_8gb_and_sdsc_csd_capacities() {
        // SDHC CSD v2.0 for an 8 GiB card: C_SIZE = 15199 (0x003b5f) -> 15200 * 1024 = 15,564,800 sectors (7600 MiB)
        let mut csd_v2 = [0u8; 16];
        csd_v2[0] = 0x40; // CSD_STRUCTURE = 1 (v2.0)
        csd_v2[7] = 0x00;
        csd_v2[8] = 0x3b;
        csd_v2[9] = 0x5f;
        let sectors = parse_csd_sectors(&csd_v2);
        assert_eq!(sectors, 15_564_800);
        assert_eq!(sectors / 2048, 7600);

        // Reject reserved CSD_STRUCTURE = 2 or 3
        let mut bad_csd = csd_v2;
        bad_csd[0] = 0x80;
        assert_eq!(parse_csd_sectors(&bad_csd), 0);
    }

    #[test]
    fn validates_asset_bundle_lba_bounds() {
        // 271,920 bytes = 532 sectors at LBA 2048 -> requires >= 2580 sectors
        assert!(asset_span_fits(SD_ASSET_LBA, 271_920, 15_564_800));
        assert!(asset_span_fits(SD_ASSET_LBA, 271_920, 2580));
        assert!(!asset_span_fits(SD_ASSET_LBA, 271_920, 2579));
        assert!(!asset_span_fits(u32::MAX - 10, 271_920, u32::MAX));
    }
}
