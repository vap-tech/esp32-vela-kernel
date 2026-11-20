//! Explicit per-service bump arena. Metadata stays on the caller's stack;
//! allocations are raw pointers and remain valid only until release/process exit.
//! Thread sharing requires userspace synchronization. No global allocator.
use core::{alloc::Layout, ptr::NonNull};
use vela_abi::syscall as abi;

use crate::sys;

pub struct Arena {
    base: usize,
    size: usize,
    used: usize,
}

impl Arena {
    pub fn new(bytes: usize) -> Result<Self, u32> {
        Self::new_in(bytes, abi::MEM_CLASS_SRAM)
    }

    pub fn new_psram(bytes: usize) -> Result<Self, u32> {
        Self::new_in(bytes, abi::MEM_CLASS_PSRAM)
    }

    pub fn new_in(bytes: usize, class: u32) -> Result<Self, u32> {
        let (base, size) = sys::call6(abi::MEM_ALLOC, bytes as u32, class, 0, 0, 0, 0);
        if size == 0 {
            return Err(base);
        }
        Ok(Self {
            base: base as usize,
            size: size as usize,
            used: 0,
        })
    }

    pub const fn base(&self) -> usize {
        self.base
    }

    pub const fn size(&self) -> usize {
        self.size
    }

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
    pub fn release(&mut self) -> Result<(), u32> {
        if self.size == 0 {
            return Ok(());
        }
        let result = sys::call(abi::MEM_FREE, self.base as u32, self.size as u32, 0, 0);
        if result != 0 {
            return Err(result);
        }
        self.base = 0;
        self.size = 0;
        self.used = 0;
        Ok(())
    }
}
