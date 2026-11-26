//! Explicit owned allocations. No global allocator or Rust alloc crate required.
use crate::{Error, Result};
use vela_user::arena::Arena;

pub struct Memory {
    arena: Arena,
    length: usize,
}

impl Memory {
    /// Allocate zeroed external PSRAM. Hardware rounds backing to 64 KiB pages.
    pub fn new(bytes: usize) -> Result<Self> {
        if bytes == 0 || bytes > u32::MAX as usize {
            return Err(Error::InvalidArgument);
        }
        let arena = Arena::new_psram(bytes).map_err(Error::Kernel)?;
        Ok(Self {
            arena,
            length: bytes,
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: this owner retains the entire allocation until Drop. SDK does
        // not expose its memory capability or APIs that release this storage.
        unsafe { core::slice::from_raw_parts(self.arena.base() as *const u8, self.length) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.arena.base() as *mut u8, self.length) }
    }

    pub fn capacity(&self) -> usize {
        self.arena.size()
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        let _ = self.arena.release();
    }
}
