//! Serialized ROM reads from the selected 16 MiB Flash. Code/stack are in SRAM.
//! HP0 kernel paths hold IRQs masked. No other runtime Flash writer exists.
pub fn read(address: u32, destination: &mut [u32]) -> Result<(), u32> {
    let bytes = destination.len().checked_mul(4).ok_or(16u32)?;
    let end = (address as usize).checked_add(bytes).ok_or(16u32)?;
    if address % 4 != 0 || bytes == 0 || end > 16 * 1024 * 1024 {
        return Err(16);
    }
    let read: extern "C" fn(u32, *mut u32, u32) -> i32 =
        unsafe { core::mem::transmute(0x4fc00158usize) };
    if read(address, destination.as_mut_ptr(), bytes as u32) != 0 {
        return Err(16);
    }
    Ok(())
}
