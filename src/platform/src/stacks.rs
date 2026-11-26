//! Write high-water marks since boot, sampled on demand, including boot activity.
const MARKER: u32 = 0x5645_4c41;
const GUARD: usize = 256;

unsafe extern "C" {
    static _hp0_stack_guard: u8;
    static _hp1_stack_guard: u8;
    static _stack_top: u8;
    static _hp1_stack_top: u8;
}

// Assembly loads deliberately sample memory another hart may currently be using.
// No Rust reference is formed to live stack storage. Results are approximate
// during concurrent execution and count writes, not uninitialized SP reservations.
fn load(address: usize) -> u32 {
    let value;
    unsafe {
        core::arch::asm!("lw {value}, 0({address})", value = out(reg) value,
            address = in(reg) address, options(nostack, readonly));
    }
    value
}

pub fn snapshot() -> [u8; 32] {
    let bounds = [
        (
            core::ptr::addr_of!(_hp0_stack_guard) as usize,
            core::ptr::addr_of!(_stack_top) as usize,
        ),
        (
            core::ptr::addr_of!(_hp1_stack_guard) as usize,
            core::ptr::addr_of!(_hp1_stack_top) as usize,
        ),
    ];
    let mut bytes = [0; 32];
    for (hart, (base, top)) in bounds.into_iter().enumerate() {
        let intact = (base..base + GUARD).step_by(4).all(|p| load(p) == MARKER);
        let mut first = base + GUARD;
        while first < top && load(first) == MARKER {
            first += 4;
        }
        let words = [
            top - base - GUARD,
            top - first,
            first - base - GUARD,
            usize::from(intact),
        ];
        for (index, value) in words.into_iter().enumerate() {
            let offset = hart * 16 + index * 4;
            bytes[offset..offset + 4].copy_from_slice(&(value as u32).to_le_bytes());
        }
    }
    bytes
}
