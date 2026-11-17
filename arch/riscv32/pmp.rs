//! Rev1.x PMP, 128-byte minimum granularity. Shared RX code + private RW stack.
//! Entry 15 explicitly denies the entire 32-bit address space. No locked entries:
//! M-mode remains unrestricted; U-mode cannot access kernel, MMIO or other stacks.
use vela_kernel::task::Region;
fn napot(r: Region) -> usize {
    assert!(r.size >= 128 && r.size.is_power_of_two() && r.base % r.size == 0);
    (r.base >> 2) | ((r.size >> 3) - 1)
}
/// # Safety
/// M-mode hart0 only, IRQs masked, no concurrent users, valid physical regions.
/// Initial ROM PMP entries must be unlocked. Readback failure stops startup.
pub unsafe fn activate(code: Region, stack: Region) -> bool {
    let code_addr = napot(code);
    let stack_addr = napot(stack);
    let (cfg, deny, addr0, addr1, unused1, unused2, all): (
        usize,
        usize,
        usize,
        usize,
        usize,
        usize,
        usize,
    );
    // SAFETY: exclusive CSR programming while no user executes. A final deny
    // entry covers all addresses before any higher-numbered inherited entries.
    unsafe {
        core::arch::asm!(
            "csrw pmpcfg0, zero", "csrw pmpcfg1, zero",
            "csrw pmpcfg2, zero", "csrw pmpcfg3, zero",
            "csrw pmpaddr0, {code}", "csrw pmpaddr1, {stack}",
            "csrw pmpaddr15, {all}",
            "csrw pmpcfg3, {deny}", "csrw pmpcfg0, {permissions}",
            "csrr {cfg}, pmpcfg0", "csrr {got_deny}, pmpcfg3",
            "csrr {addr0}, pmpaddr0", "csrr {addr1}, pmpaddr1",
            "csrr {unused1}, pmpcfg1", "csrr {unused2}, pmpcfg2", "csrr {got_all}, pmpaddr15",
            "fence rw, rw", "fence.i",
            code=in(reg)code_addr, stack=in(reg)stack_addr,
            all=in(reg)0x1fff_ffffusize, deny=in(reg)0x1800_0000usize,
            permissions=in(reg)0x1b1dusize,
            cfg=lateout(reg)cfg, got_deny=lateout(reg)deny,
            addr0=lateout(reg)addr0, addr1=lateout(reg)addr1,
            unused1=lateout(reg)unused1, unused2=lateout(reg)unused2, got_all=lateout(reg)all,
            options(nostack));
    }
    cfg == 0x1b1d
        && deny == 0x1800_0000
        && addr0 == code_addr
        && addr1 == stack_addr
        && unused1 == 0
        && unused2 == 0
        && all == 0x1fff_ffff
}
