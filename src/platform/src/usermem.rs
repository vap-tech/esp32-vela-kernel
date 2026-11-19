//! Checked copies across the U-mode boundary.
//! Every transfer validates the target `AddressSpace` and uses raw pointers
//! under single-hart masked-IRQ execution so no Rust reference ever aliases
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
    // SAFETY: entire readable user range verified against `space`; single hart, MIE=0.
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
    // SAFETY: entire writable user range verified against `space`; single hart, MIE=0.
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
    // SAFETY: both ranges validated in physical SRAM; distinct processes in RPC, MIE=0.
    unsafe {
        core::ptr::copy(src_ptr as *const u8, dst_ptr as *mut u8, length);
    }
    Ok(())
}
