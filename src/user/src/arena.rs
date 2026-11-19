//! Explicit per-service bump arena. Metadata stays on the caller's stack;
//! allocations are raw pointers and remain valid only until release/process exit.
//! Thread sharing requires userspace synchronization. No global allocator.
use core::{alloc::Layout, ptr::NonNull};
use vela_abi::syscall as abi;

pub struct Arena {
    base: usize,
    size: usize,
    used: usize,
}
impl Arena {
    #[inline(always)]
    pub fn new(bytes: usize) -> Result<Self, u32> {
        let (base, size) = super::call6(abi::MEM_ALLOC, bytes as u32, 0, 0, 0, 0, 0);
        if (base as i32) < 0 {
            return Err(base);
        }
        Ok(Self {
            base: base as usize,
            size: size as usize,
            used: 0,
        })
    }
    #[inline(always)]
    pub fn allocate(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let start = self.base.checked_add(self.used)?;
        let aligned = start.checked_add(layout.align() - 1)? & !(layout.align() - 1);
        let end = aligned.checked_add(layout.size().max(1))?;
        let used = end.checked_sub(self.base)?;
        if used > self.size {
            return None;
        }
        self.used = used;
        NonNull::new(aligned as *mut u8)
    }
    /// On EBUSY (e.g. a live thread stack), retain ownership for a later retry.
    #[inline(always)]
    pub fn release(&mut self) -> Result<(), u32> {
        if self.size == 0 {
            return Ok(());
        }
        let result = super::call(abi::MEM_FREE, self.base as u32, self.size as u32, 0, 0);
        if result != 0 {
            return Err(result);
        }
        self.base = 0;
        self.size = 0;
        self.used = 0;
        Ok(())
    }
}
