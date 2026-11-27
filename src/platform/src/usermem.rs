//! Checked copies across the U-mode boundary.
//! Every transfer validates the target `AddressSpace` and uses raw pointers
//! under KERNEL_LOCK with local IRQs masked so no Rust reference ever aliases
//! user memory.

use vela_kernel::{
    errno::Errno,
    process::{AddressSpace, READ, WRITE},
};

#[inline(always)]
unsafe fn raw_copy(mut src: *const u8, mut dst: *mut u8, mut len: usize) {
    unsafe {
        if ((src as usize) | (dst as usize)) & 3 == 0 {
            while len >= 4 {
                (dst as *mut u32).write((src as *const u32).read());
                src = src.add(4);
                dst = dst.add(4);
                len -= 4;
            }
        }
        while len > 0 {
            dst.write(src.read());
            src = src.add(1);
            dst = dst.add(1);
            len -= 1;
        }
    }
}

pub fn copy_from_user(space: &AddressSpace, pointer: usize, dst: &mut [u8]) -> Result<(), Errno> {
    if !space.permits(pointer, dst.len(), READ) {
        return Err(Errno::Efault);
    }
    // SAFETY: entire readable range verified, private destination, KERNEL_LOCK held.
    unsafe {
        raw_copy(pointer as *const u8, dst.as_mut_ptr(), dst.len());
    }
    Ok(())
}

pub fn copy_to_user(space: &AddressSpace, pointer: usize, src: &[u8]) -> Result<(), Errno> {
    copy_to_user_within(space, pointer, src.len(), src)
}

/// Validates the full caller-provided `capacity` as writable, then copies `src`.
/// Used by IPC receive/reply paths where the entire receive buffer must remain mapped.
pub fn copy_to_user_within(
    space: &AddressSpace,
    pointer: usize,
    capacity: usize,
    src: &[u8],
) -> Result<(), Errno> {
    if src.len() > capacity || !space.permits(pointer, capacity, WRITE) {
        return Err(Errno::Efault);
    }
    // SAFETY: entire writable range verified, private source, KERNEL_LOCK held.
    unsafe {
        raw_copy(src.as_ptr(), pointer as *mut u8, src.len());
    }
    Ok(())
}

/// Validate `src_space` readability and `dst_space` writability (`capacity`), then
/// copy `length` bytes directly between user buffers without intermediate stack zeroing.
pub fn copy_between_users(
    src_space: &AddressSpace,
    src_ptr: usize,
    dst_space: &AddressSpace,
    dst_ptr: usize,
    dst_capacity: usize,
    length: usize,
) -> Result<(), Errno> {
    if length > dst_capacity
        || !src_space.permits(src_ptr, length, READ)
        || !dst_space.permits(dst_ptr, dst_capacity, WRITE)
    {
        return Err(Errno::Efault);
    }
    // SAFETY: both complete ranges validated in SRAM/PSRAM; KERNEL_LOCK prevents
    // mapping changes. memmove semantics also cover overlapping shared mappings.
    unsafe {
        core::ptr::copy(src_ptr as *const u8, dst_ptr as *mut u8, length);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vela_kernel::task::Region;

    #[repr(align(512))]
    struct Page([u8; 512]);

    fn mapping(page: &Page, permissions: u8) -> AddressSpace {
        let mut space = AddressSpace::EMPTY;
        space
            .insert(
                Region {
                    base: page.0.as_ptr() as usize,
                    size: 512,
                },
                permissions,
            )
            .unwrap();
        space
    }

    #[test]
    fn copies_every_alignment_and_tail_without_touching_neighbors() {
        let mut src = Page([0; 512]);
        for (i, byte) in src.0.iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37);
        }
        let src_space = mapping(&src, READ);
        let mut dst = Page([0; 512]);
        let dst_space = mapping(&dst, READ | WRITE);
        for from in 0..8 {
            for to in 0..8 {
                for len in 0..=129 {
                    dst.0.fill(0xa5);
                    copy_between_users(
                        &src_space,
                        src.0.as_ptr() as usize + from,
                        &dst_space,
                        dst.0.as_mut_ptr() as usize + to,
                        len,
                        len,
                    )
                    .unwrap();
                    assert_eq!(&dst.0[to..to + len], &src.0[from..from + len]);
                    assert!(dst.0[..to].iter().all(|&b| b == 0xa5));
                    assert!(dst.0[to + len..].iter().all(|&b| b == 0xa5));
                }
            }
        }
    }

    #[test]
    fn shared_mapping_overlap_matches_memmove_in_both_directions() {
        let mut page = Page([0; 512]);
        let space = mapping(&page, READ | WRITE);
        let mut original = [0; 512];
        for (i, byte) in original.iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(19);
        }
        for from in 0..24 {
            for to in 0..24 {
                for len in [0, 1, 3, 4, 15, 16, 17, 31, 32, 65, 129] {
                    page.0 = original;
                    let mut expected = original;
                    expected.copy_within(from..from + len, to);
                    copy_between_users(
                        &space,
                        page.0.as_ptr() as usize + from,
                        &space,
                        page.0.as_mut_ptr() as usize + to,
                        len,
                        len,
                    )
                    .unwrap();
                    assert_eq!(page.0, expected);
                }
            }
        }
    }

    #[test]
    fn rejects_invalid_ranges_and_permissions_before_copying() {
        let src = Page([0x5a; 512]);
        let mut dst = Page([0xa5; 512]);
        let src_space = mapping(&src, READ);
        let dst_space = mapping(&dst, READ | WRITE);
        let src_ptr = src.0.as_ptr() as usize;
        let dst_ptr = dst.0.as_mut_ptr() as usize;
        for (from, to, capacity, len) in [
            (src_ptr + 511, dst_ptr, 2, 2),
            (src_ptr, dst_ptr + 511, 2, 1),
            (src_ptr, dst_ptr, 3, 4),
            (usize::MAX - 1, dst_ptr, 4, 4),
        ] {
            assert_eq!(
                copy_between_users(&src_space, from, &dst_space, to, capacity, len),
                Err(Errno::Efault)
            );
            assert_eq!(dst.0, [0xa5; 512]);
        }
        let readonly = mapping(&dst, READ);
        assert_eq!(
            copy_between_users(&src_space, src_ptr, &readonly, dst_ptr, 16, 16),
            Err(Errno::Efault)
        );
        assert_eq!(dst.0, [0xa5; 512]);
    }

    #[test]
    fn kernel_user_copies_validate_capacity_and_round_trip_tails() {
        let mut page = Page([0xa5; 512]);
        let space = mapping(&page, READ | WRITE);
        let base = page.0.as_mut_ptr() as usize;
        let input = [0x5a; 129];
        copy_to_user(&space, base, &input).unwrap();
        let mut output = [0; 129];
        copy_from_user(&space, base, &mut output).unwrap();
        assert_eq!(output, input);
        assert_eq!(page.0[129..], [0xa5; 383]);
        assert_eq!(
            copy_to_user_within(&space, base + 511, 2, &[1]),
            Err(Errno::Efault)
        );
        assert_eq!(page.0[511], 0xa5);
        assert_eq!(
            copy_from_user(&space, base + 511, &mut output),
            Err(Errno::Efault)
        );
        assert_eq!(output, input);
    }
}
