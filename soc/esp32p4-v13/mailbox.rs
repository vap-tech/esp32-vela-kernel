//! ESP32-P4 v1.3 TRM 22.3. Word writes latch the matching interrupt bit.
pub const BASE: usize = 0x5011_8000;
pub fn word(index: usize) -> *mut u32 {
    (BASE + 4 * index) as *mut u32
}
