//! Bounded ELF32 little-endian RISC-V ET_EXEC validation. No memory is changed
//! until the whole image has passed validation. Dynamic linking is unsupported.
use crate::task::Region;

pub const MAX_SEGMENTS: usize = 8;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Header,
    Unsupported,
    Bounds,
    Segment,
    Overlap,
    Entry,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Segment {
    pub region: Region,
    pub file_offset: usize,
    pub file_size: usize,
    /// ELF PF_X=1, PF_W=2, PF_R=4, not kernel mapping flags.
    pub flags: u32,
}
pub struct Image<'a> {
    bytes: &'a [u8],
    pub entry: usize,
    segments: [Segment; MAX_SEGMENTS],
    count: usize,
}
fn word(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let b = bytes
        .get(offset..offset.checked_add(4).ok_or(Error::Bounds)?)
        .ok_or(Error::Bounds)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
fn half(bytes: &[u8], offset: usize) -> Result<u16, Error> {
    let b = bytes
        .get(offset..offset.checked_add(2).ok_or(Error::Bounds)?)
        .ok_or(Error::Bounds)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}
impl<'a> Image<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() < 52 || bytes.get(..7) != Some(b"\x7fELF\x01\x01\x01") {
            return Err(Error::Header);
        }
        if half(bytes, 16)? != 2
            || half(bytes, 18)? != 243
            || word(bytes, 20)? != 1
            || half(bytes, 40)? != 52
            || half(bytes, 42)? != 32
        {
            return Err(Error::Unsupported);
        }
        // RVC allowed; floating-point ABI, RVE and unknown extension flags rejected.
        if word(bytes, 36)? & !1 != 0 {
            return Err(Error::Unsupported);
        }
        let offset = word(bytes, 28)? as usize;
        let count = half(bytes, 44)? as usize;
        if count == 0 || count > 32 {
            return Err(Error::Bounds);
        }
        let end = offset.checked_add(count * 32).ok_or(Error::Bounds)?;
        if offset < 52 || end > bytes.len() {
            return Err(Error::Bounds);
        }
        let mut image = Self {
            bytes,
            entry: word(bytes, 24)? as usize,
            segments: [Segment::default(); MAX_SEGMENTS],
            count: 0,
        };
        for i in 0..count {
            let p = offset + i * 32;
            let kind = word(bytes, p)?;
            if kind == 2 || kind == 3 || kind == 7 {
                return Err(Error::Unsupported);
            }
            if kind != 1 {
                continue;
            }
            let file_offset = word(bytes, p + 4)? as usize;
            let base = word(bytes, p + 8)? as usize;
            let physical = word(bytes, p + 12)? as usize;
            let file_size = word(bytes, p + 16)? as usize;
            let size = word(bytes, p + 20)? as usize;
            let flags = word(bytes, p + 24)?;
            let align = word(bytes, p + 28)? as usize;
            if size == 0 {
                if file_size != 0 {
                    return Err(Error::Segment);
                }
                continue;
            }
            if image.count == MAX_SEGMENTS
                || base == 0
                || physical != base
                || file_size > size
                || flags & !7 != 0
                || flags & 4 == 0
                || flags & 3 == 3
                || (align > 1 && (!align.is_power_of_two() || base % align != file_offset % align))
            {
                return Err(Error::Segment);
            }
            // Validate RV32 ranges even when this parser is tested on a 64-bit host.
            if (base as u32).checked_add(size as u32).is_none()
                || file_offset
                    .checked_add(file_size)
                    .is_none_or(|e| e > bytes.len())
            {
                return Err(Error::Bounds);
            }
            let region = Region { base, size };
            for other in image.segments() {
                if base < other.region.base + other.region.size && other.region.base < base + size {
                    return Err(Error::Overlap);
                }
            }
            image.segments[image.count] = Segment {
                region,
                file_offset,
                file_size,
                flags,
            };
            image.count += 1;
        }
        if image.entry & 1 != 0
            || !image.segments().iter().any(|s| {
                s.flags & 1 != 0
                    && image.entry >= s.region.base
                    && image.entry < s.region.base + s.file_size
            })
        {
            return Err(Error::Entry);
        }
        Ok(image)
    }
    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.count]
    }
    /// File-backed bytes only; the caller must zero region.size - file_size BSS.
    pub fn data(&self, segment: usize) -> Option<&'a [u8]> {
        let s = self.segments().get(segment)?;
        self.bytes.get(s.file_offset..s.file_offset + s.file_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn executable() -> [u8; 88] {
        let mut b = [0; 88];
        b[..7].copy_from_slice(b"\x7fELF\x01\x01\x01");
        b[16..18].copy_from_slice(&2u16.to_le_bytes());
        b[18..20].copy_from_slice(&243u16.to_le_bytes());
        for (o, v) in [
            (20, 1u32),
            (24, 0x1000),
            (28, 52),
            (52, 1),
            (56, 84),
            (60, 0x1000),
            (64, 0x1000),
            (68, 4),
            (72, 8),
            (76, 5),
            (80, 4),
        ] {
            b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
        for (o, v) in [(40, 52u16), (42, 32), (44, 1)] {
            b[o..o + 2].copy_from_slice(&v.to_le_bytes());
        }
        b
    }
    #[test]
    fn accepts_code_with_bss() {
        let b = executable();
        let image = Image::parse(&b).unwrap();
        assert_eq!(image.entry, 0x1000);
        assert_eq!(image.segments()[0].region.size, 8);
        assert_eq!(image.data(0).unwrap().len(), 4);
        assert!(image.data(1).is_none());
    }
    #[test]
    fn rejects_all_truncated_inputs() {
        let b = executable();
        for n in 0..b.len() {
            assert!(Image::parse(&b[..n]).is_err());
        }
    }
    #[test]
    fn rejects_overlapping_and_wrapping_segments() {
        let mut b = [0u8; 120];
        b[..88].copy_from_slice(&executable());
        b[44..46].copy_from_slice(&2u16.to_le_bytes());
        for (o, v) in [
            (84, 1u32),
            (88, 116),
            (92, 0x1004),
            (96, 0x1004),
            (100, 4),
            (104, 8),
            (108, 6),
            (112, 4),
        ] {
            b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        }
        assert!(matches!(Image::parse(&b), Err(Error::Overlap)));
        for o in [92, 96] {
            b[o..o + 4].copy_from_slice(&0xffff_fffcu32.to_le_bytes());
        }
        assert!(matches!(Image::parse(&b), Err(Error::Bounds)));
    }
    #[test]
    fn rejects_unsafe_layouts() {
        for (offset, value, error) in [
            (76, 7, Error::Segment),
            (72, 2, Error::Segment),
            (24, 0x1004, Error::Entry),
            (56, u32::MAX, Error::Segment),
            (60, 0xffff_fffc, Error::Segment),
            (36, 4, Error::Unsupported),
            (52, 3, Error::Unsupported),
        ] {
            let mut b = executable();
            b[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(matches!(Image::parse(&b), Err(e) if e == error));
        }
    }
}
