//! Bounded ELF32 little-endian RISC-V ET_EXEC and static PIE validation. No memory is changed
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
    pub pic: bool,
    dynamic: Option<(usize, usize, usize)>,
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
        let kind = half(bytes, 16)?;
        if !matches!(kind, 2 | 3)
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
            pic: kind == 3,
            dynamic: None,
            entry: word(bytes, 24)? as usize,
            segments: [Segment::default(); MAX_SEGMENTS],
            count: 0,
        };
        for i in 0..count {
            let p = offset + i * 32;
            let kind = word(bytes, p)?;
            if kind == 2 && image.pic {
                let header = (
                    word(bytes, p + 4)? as usize,
                    word(bytes, p + 8)? as usize,
                    word(bytes, p + 16)? as usize,
                );
                if image.dynamic.replace(header).is_some() {
                    return Err(Error::Unsupported);
                }
                continue;
            }
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
                || (!image.pic && base == 0)
                || (image.pic && align > 4096)
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
        image.visit_relocations(|_, _| {})?;
        Ok(image)
    }
    fn file_range(&self, addr: usize, len: usize) -> Result<usize, Error> {
        for s in self.segments() {
            if let Some(delta) = addr.checked_sub(s.region.base) {
                if delta <= s.file_size && len <= s.file_size - delta {
                    return Ok(s.file_offset + delta);
                }
            }
        }
        Err(Error::Bounds)
    }
    /// Apply only symbol-free RV32 RELATIVE relocations to aligned RW words.
    /// Offsets and addends refer to original virtual addresses, before rebasing.
    pub fn visit_relocations(&self, mut apply: impl FnMut(usize, usize)) -> Result<(), Error> {
        let Some((offset, addr, size)) = self.dynamic else {
            return Ok(());
        };
        if size == 0 || size % 8 != 0 || size > 512 || self.file_range(addr, size)? != offset {
            return Err(Error::Bounds);
        }
        let mut rela = None;
        let mut length = None;
        let mut stride = None;
        let mut terminated = false;
        for p in (offset..offset + size).step_by(8) {
            let value = word(self.bytes, p + 4)? as usize;
            match word(self.bytes, p)? {
                0 => {
                    terminated = true;
                    break;
                }
                7 if rela.replace(value).is_none() => (),
                8 if length.replace(value).is_none() => (),
                9 if stride.replace(value).is_none() => (),
                0x6ffffffb if value == 0x08000000 => (),
                4 | 5 | 6 | 10 | 11 | 21 | 0x6ffffff9 | 0x6ffffef5 => (),
                _ => return Err(Error::Unsupported),
            }
        }
        if !terminated {
            return Err(Error::Header);
        }
        let (address, size) = match (rela, length, stride) {
            (None, None, None) => return Ok(()),
            (Some(a), Some(n), Some(12)) if n % 12 == 0 && n <= 12 * 4096 => (a, n),
            _ => return Err(Error::Unsupported),
        };
        let start = self.file_range(address, size)?;
        for p in (start..start + size).step_by(12) {
            let target = word(self.bytes, p)? as usize;
            let addend = word(self.bytes, p + 8)? as usize;
            if word(self.bytes, p + 4)? != 3
                || target % 4 != 0
                || !self
                    .segments()
                    .iter()
                    .any(|s| s.flags & 2 != 0 && s.region.contains(target, 4))
                || !self.segments().iter().any(|s| s.region.contains(addend, 1))
            {
                return Err(Error::Unsupported);
            }
            apply(target, addend);
        }
        Ok(())
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

/// Cover a PIE segment `[vaddr .. vaddr + size)` with up to 6 contiguous,
/// naturally aligned power-of-two NAPOT regions (`>= 128 B` each).
/// Handles segments whose `.data + .bss` exceeds their start-address alignment
/// (e.g. `.data` at `0x1000` with a 4 KiB `.bss` -> `[0x1000, 4K]` + `[0x2000, 128B]`).
pub fn tile_pie_segment(vaddr: usize, size: usize) -> Result<([Region; 6], usize), Error> {
    if size == 0 || (vaddr & 127) != 0 {
        return Err(Error::Segment);
    }
    let mut out = [Region { base: 0, size: 0 }; 6];
    let mut count = 0usize;
    let mut cur = vaddr;
    let target = vaddr.checked_add(size).ok_or(Error::Bounds)?;
    while cur < target {
        if count >= out.len() {
            return Err(Error::Segment);
        }
        let rem = target - cur;
        let align_limit = if cur == 0 {
            usize::MAX
        } else {
            1usize << cur.trailing_zeros()
        };
        if align_limit < 128 {
            return Err(Error::Segment);
        }
        let need = rem
            .max(128)
            .checked_next_power_of_two()
            .ok_or(Error::Bounds)?;
        let chunk = if need <= align_limit {
            need
        } else {
            align_limit
        };
        let next = cur.checked_add(chunk).ok_or(Error::Bounds)?;
        out[count] = Region {
            base: cur,
            size: chunk,
        };
        count += 1;
        cur = next;
    }
    Ok((out, count))
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
    fn relative_relocation_requires_private_writable_word_and_local_addend() {
        let mut bytes = [0u8; 64];
        // A read-only RELA record followed by a writable segment containing DT_*.
        for (offset, value) in [
            (0, 0x1000u32),
            (4, 3),
            (8, 4),
            (16, 7),
            (20, 0),
            (24, 8),
            (28, 12),
            (32, 9),
            (36, 12),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (field, value, valid) in [
            (0, 0x1000u32, true),
            (0, 0, false),
            (0, 0x1001, false),
            (4, 0x103, false),
            (8, 0xffff, false),
        ] {
            let mut input = bytes;
            input[field..field + 4].copy_from_slice(&value.to_le_bytes());
            let mut segments = [Segment::default(); MAX_SEGMENTS];
            segments[0] = Segment {
                region: Region { base: 0, size: 16 },
                file_size: 16,
                file_offset: 0,
                flags: 5,
            };
            segments[1] = Segment {
                region: Region {
                    base: 0x1000,
                    size: 48,
                },
                file_size: 48,
                file_offset: 16,
                flags: 6,
            };
            let image = Image {
                bytes: &input,
                entry: 0,
                pic: true,
                dynamic: Some((16, 0x1000, 32)),
                segments,
                count: 2,
            };
            let mut patch = None;
            assert_eq!(
                image.visit_relocations(|a, b| patch = Some((a, b))).is_ok(),
                valid
            );
            assert_eq!(patch, valid.then_some((0x1000, 4)));
        }
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

    #[test]
    fn tiles_pie_segments_into_minimal_napot_regions() {
        // Text at vaddr=0 fits in a single NAPOT slot.
        let (r, n) = tile_pie_segment(0, 0x8d8).unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            r[0],
            Region {
                base: 0,
                size: 0x1000
            }
        );

        // Data+BSS at 0x1000 exceeding 4 KiB splits into 4 KiB + 128 B.
        let (r, n) = tile_pie_segment(0x1000, 0x1048).unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            r[0],
            Region {
                base: 0x1000,
                size: 0x1000
            }
        );
        assert_eq!(
            r[1],
            Region {
                base: 0x2000,
                size: 0x80
            }
        );

        // Large 64 KiB + 72 B BSS at 0x1000 fits in 5 NAPOT slots.
        let (r, n) = tile_pie_segment(0x1000, 0x10048).unwrap();
        assert_eq!(n, 5);
        for slot in &r[..n] {
            assert!(slot.size >= 128 && slot.size.is_power_of_two());
            assert_eq!(slot.base % slot.size, 0);
        }
        assert_eq!(r[0].base, 0x1000);
        assert!(r[n - 1].base + r[n - 1].size >= 0x11048);

        // Unaligned vaddr or zero size rejected.
        assert_eq!(tile_pie_segment(0x40, 0x100), Err(Error::Segment));
        assert_eq!(tile_pie_segment(0, 0), Err(Error::Segment));
    }
}
