//! P4 CLIC context: mcause includes previous interrupt level and privilege.
//! mscratch is zero in kernel, kernel stack top in U-mode. Nesting stays disabled.
core::arch::global_asm!(include_str!("trap.S"));

/// Initial `mstatus.FS` (`Clean` = `2 << 13`) for user threads so `RV32F` instructions
/// execute in U-mode without trapping.
pub const MSTATUS_FS_MASK: u32 = 3 << 13;
pub const MSTATUS_FS_CLEAN: u32 = 2 << 13;

/// Initial `CSR 0x7f2` (`PIE_STATE = Clean = 2`) for user threads so `ESP32-P4 v1.3`
/// `Xespv2p1` PIE SIMD/accumulator instructions execute in U-mode without trapping.
pub const PIE_STATE_OFF: u32 = 0;
pub const PIE_STATE_CLEAN: u32 = 2;

#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct TrapFrame {
    // ra,gp,tp,t0,t1,t2,s0,s1,a0..a7,s2..s11,t3..t6
    pub registers: [u32; 30],
    pub sp: u32,
    pub mcause: u32,
    pub mepc: u32,
    pub mtval: u32,
    pub mstatus: u32,
    pub padding: u32,
}

/// Per-thread extended architectural state for `RV32F` FPU (`f0..f31`, `fcsr`) and
/// `ESP32-P4 v1.3` (`Xespv2p1`) PIE (`q0..q7`, `QACC_L/H`, `UA_STATE`, `XACC`, `SAR`,
/// `SAR_BYTES`, `FFT_BIT_WIDTH`, `CFG`, and `CSR 0x7f2`).
///
/// Must remain 16-byte aligned in internal L2MEM SRAM (`0x4ff0_0000..0x4ffc_0000`)
/// for 128-bit PIE vector/accumulator loads and stores (`esp.vld.128.ip` / `esp.vst.128.ip`).
#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct ExtendedContext {
    /// 128-bit PIE vector registers `q0..q7` (128 bytes, offset `0`).
    pub q: [[u8; 16]; 8],
    /// 512-bit PIE vector accumulator `QACC` (`L_L`, `L_H`, `H_L`, `H_H`: 64 bytes, offset `128`).
    pub qacc: [[u8; 16]; 4],
    /// 128-bit PIE unaligned load/store state `UA_STATE` (16 bytes, offset `192`).
    pub ua_state: [u8; 16],
    /// 40-bit scalar accumulator `XACC` low 32 bits (`offset 208`).
    pub xacc_lo: u32,
    /// 40-bit scalar accumulator `XACC` high 8 bits (`offset 212`).
    pub xacc_hi: u32,
    /// Shift amount register `SAR` (`offset 216`).
    pub sar: u32,
    /// Byte shift register `SAR_BYTES` (`offset 220`).
    pub sar_bytes: u32,
    /// FFT bit width register `FFT_BIT_WIDTH` (`offset 224`).
    pub fft_bit_width: u32,
    /// PIE configuration register `CFG` (`offset 228`).
    pub pie_cfg: u32,
    /// Saved `CSR 0x7f2` (`PIE` state: `2 = Clean`, `3 = Dirty`; `offset 232`).
    pub pie_state: u32,
    pub reserved: u32,
    /// Single-precision floating-point registers `f0..f31` (128 bytes, offset `240`).
    pub f: [u32; 32],
    /// Floating-point control and status register `fcsr` (`offset 368`).
    pub fcsr: u32,
    pub pad: [u32; 3],
}

impl ExtendedContext {
    pub const ZERO: Self = Self {
        q: [[0; 16]; 8],
        qacc: [[0; 16]; 4],
        ua_state: [0; 16],
        xacc_lo: 0,
        xacc_hi: 0,
        sar: 0,
        sar_bytes: 0,
        fft_bit_width: 0,
        pie_cfg: 0,
        pie_state: PIE_STATE_CLEAN,
        reserved: 0,
        f: [0; 32],
        fcsr: 0,
        pad: [0; 3],
    };
}

impl TrapFrame {
    pub fn user(entry: usize, sp: usize, argument: u32) -> Self {
        Self::user_args(entry, sp, [argument, 0, 0, 0, 0, 0])
    }

    pub fn user_args(entry: usize, sp: usize, args: [u32; 6]) -> Self {
        let mut frame = Self {
            registers: [0; 30],
            sp: sp as u32,
            mcause: 1 << 27,
            mepc: entry as u32,
            mtval: 0,
            mstatus: (1 << 7) | MSTATUS_FS_CLEAN,
            padding: 0,
        };
        frame.registers[8..14].copy_from_slice(&args);
        frame
    }
}
unsafe extern "C" {
    pub fn vela_idle_entry();
    pub fn vela_hp1_entry() -> !;
    fn vela_resume(frame: *const TrapFrame) -> !;
    fn vela_ext_save(ctx: *mut ExtendedContext);
    fn vela_ext_restore(ctx: *const ExtendedContext);
}
/// # Safety
/// Local HP hart M-mode, IRQs disabled, valid user mappings and kernel stack, complete
/// integer frame; MPP=U or the dedicated M-mode idle continuation.
pub unsafe fn enter(frame: *const TrapFrame) -> ! {
    unsafe { vela_resume(frame) }
}

/// Save the calling HP hart's live `RV32F` FPU (`f0..f31`, `fcsr`) and `ESP32-P4 v1.3`
/// `PIE` (`q0..q7`, `QACC`, `UA_STATE`, `XACC`, `SAR`, `SAR_BYTES`, `FFT_BIT_WIDTH`, `CFG`)
/// registers into `ctx`, leaving `mstatus.FS = Off` and `CSR 0x7f2 = Off`.
///
/// # Safety
/// Must be called in M-mode with IRQs masked and `ctx` pointing to a 16-byte-aligned
/// [`ExtendedContext`] in internal L2MEM SRAM.
#[inline(always)]
pub unsafe fn save_extended_context(ctx: &mut ExtendedContext, pie_state: u32) {
    ctx.pie_state = if pie_state == 0 {
        PIE_STATE_CLEAN
    } else {
        pie_state
    };
    unsafe { vela_ext_save(ctx as *mut ExtendedContext) };
}

/// Restore the calling HP hart's `RV32F` FPU and `ESP32-P4 v1.3` `PIE` registers from `ctx`,
/// leaving `CSR 0x7f2 = ctx.pie_state` (or `Clean`) ready for U-mode execution.
///
/// # Safety
/// Must be called in M-mode with IRQs masked right before returning to U-mode, with `ctx`
/// pointing to a 16-byte-aligned [`ExtendedContext`] in internal L2MEM SRAM.
#[inline(always)]
pub unsafe fn restore_extended_context(ctx: &ExtendedContext) {
    unsafe { vela_ext_restore(ctx as *const ExtendedContext) };
}

/// Read current `CSR 0x7f2` (`PIE` state) and set it to `0` (`Off`) while in M-mode.
///
/// # Safety
/// Must be called in M-mode on an ESP32-P4 HP hart.
#[inline(always)]
pub unsafe fn enter_kernel_pie_off() -> u32 {
    let prev: u32;
    unsafe {
        core::arch::asm!("csrrwi {}, 0x7f2, 0", out(reg) prev, options(nomem, nostack));
    }
    prev
}

/// Restore `CSR 0x7f2` (`PIE` state) right before returning to the same user thread.
///
/// # Safety
/// Must be called in M-mode on an ESP32-P4 HP hart.
#[inline(always)]
pub unsafe fn exit_kernel_pie_restore(state: u32) {
    let val = if state == 0 { PIE_STATE_CLEAN } else { state };
    unsafe {
        core::arch::asm!("csrw 0x7f2, {}", in(reg) val, options(nomem, nostack));
    }
}

/// Disable `CSR 0x7f2` (`PIE = Off`) when entering the M-mode idle loop.
///
/// # Safety
/// Must be called in M-mode on an ESP32-P4 HP hart.
#[inline(always)]
pub unsafe fn disable_pie() {
    unsafe {
        core::arch::asm!("csrwi 0x7f2, 0", options(nomem, nostack));
    }
}

pub fn hart_id() -> usize {
    let id;
    // SAFETY: M-mode CSR read only.
    unsafe {
        core::arch::asm!("csrr {}, mhartid", out(reg) id, options(nomem, nostack));
    }
    id
}
pub fn halt() -> ! {
    loop {
        core::hint::spin_loop();
    }
}
const _: () = assert!(core::mem::size_of::<TrapFrame>() == 144);
const _: () = assert!(core::mem::size_of::<ExtendedContext>() == 384);
const _: () = assert!(core::mem::align_of::<ExtendedContext>() == 16);

/// Enable `mcycle` (`mcountinhibit.CY = 0`) and permit unprivileged `rdcycle`/`rdcycleh`
/// reads (`mcounteren.CY = 1`) on the calling HP hart.
///
/// # Safety
/// Must be called in M-mode on the active HP hart.
pub unsafe fn init_counters() {
    unsafe {
        core::arch::asm!("csrci 0x320, 1", "csrsi 0x306, 1", options(nomem, nostack));
    }
}

/// Read the 64-bit hardware `mcycle` counter without high/low word tearing on RV32.
pub fn mcycle() -> u64 {
    // SAFETY: side-effect-free M-mode CSR reads.
    unsafe {
        loop {
            let hi1: u32;
            let lo: u32;
            let hi2: u32;
            core::arch::asm!(
                "csrr {0}, mcycleh",
                "csrr {1}, mcycle",
                "csrr {2}, mcycleh",
                out(reg) hi1,
                out(reg) lo,
                out(reg) hi2,
                options(nomem, nostack)
            );
            if hi1 == hi2 {
                return (u64::from(hi1) << 32) | u64::from(lo);
            }
        }
    }
}
