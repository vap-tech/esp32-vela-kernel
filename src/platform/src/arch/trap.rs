//! P4 CLIC context: mcause includes previous interrupt level and privilege.
//! mscratch is zero in kernel, kernel stack top in U-mode. Nesting stays disabled.
core::arch::global_asm!(include_str!("trap.S"));
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
            mstatus: 1 << 7,
            padding: 0,
        };
        frame.registers[8..14].copy_from_slice(&args);
        frame
    }
}
unsafe extern "C" {
    pub fn vela_idle_entry();
    fn vela_resume(frame: *const TrapFrame) -> !;
}
/// # Safety
/// Hart0 M-mode, IRQs disabled, valid user mappings and kernel stack, complete
/// integer frame with FS=off; MPP=U or the dedicated M-mode idle continuation.
pub unsafe fn enter(frame: *const TrapFrame) -> ! {
    unsafe { vela_resume(frame) }
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
