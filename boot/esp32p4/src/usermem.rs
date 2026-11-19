//! Checked byte-wise copies across the U-mode boundary.
//! Every transfer validates the target `AddressSpace` and uses raw pointers
//! under single-hart masked-IRQ execution so no Rust reference ever aliases
//! user memory.

use vela_kernel::{
    errno::Errno,
    process::{AddressSpace, READ, WRITE},
};

pub fn copy_from_user(space: &AddressSpace, pointer: usize, dst: &mut [u8]) -> Result<(), Errno> {
    if !space.permits(pointer, dst.len(), READ) {
        return Err(Errno::Efault);
    }
    for (i, slot) in dst.iter_mut().enumerate() {
        // SAFETY: entire readable user range verified against `space`; single hart, MIE=0.
        *slot = unsafe { (pointer as *const u8).add(i).read() };
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
    for (i, &byte) in src.iter().enumerate() {
        // SAFETY: entire writable user range verified against `space`; single hart, MIE=0.
        unsafe {
            (pointer as *mut u8).add(i).write(byte);
        }
    }
    Ok(())
}
