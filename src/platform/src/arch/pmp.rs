//! P4 rev1.3: 16 entries, minimum 128B, no NA4. Never lock dynamic mappings.
//! Entries 0..7 hold the active process's up to 8 regions (`pmpcfg0..1`);
//! entries 8..14 stay 0; entry 15 holds the permanent full-address-space
//! U-mode deny-all catch-all.
use vela_kernel::{
    errno::Errno,
    process::{AddressSpace, REGIONS},
};

const _: () = assert!(REGIONS == 8);
const DENY_ALL_ADDR: usize = 0x1fff_ffff;
const DENY_ALL_CFG3: usize = 0x1800_0000;

#[derive(Clone, Copy)]
pub struct Prepared {
    address: [usize; REGIONS],
    config: [usize; 2],
}

impl Prepared {
    pub fn new(memory: &AddressSpace) -> Result<Self, Errno> {
        let mut result = Self {
            address: [0; REGIONS],
            config: [0; 2],
        };
        for (index, mapping) in memory.maps.iter().enumerate() {
            let r = mapping.region;
            if r.size == 0 {
                continue;
            }
            if r.size < 128 || !r.size.is_power_of_two() || r.base % r.size != 0 {
                return Err(Errno::Einval);
            }
            result.address[index] = (r.base >> 2) | ((r.size >> 3) - 1);
            result.config[index / 4] |=
                (0x18 | usize::from(mapping.permissions)) << ((index % 4) * 8);
        }
        Ok(result)
    }

    /// Initialize all 16 PMP entries at boot and verify full CSR readback.
    ///
    /// # Safety
    /// Local hart M-mode, IRQs masked, no user execution.
    pub unsafe fn activate_initial(&self) -> bool {
        let mut observed_addr = [0usize; 16];
        let mut observed_cfg = [0usize; 4];
        unsafe {
            core::arch::asm!(
                "csrw pmpcfg0, zero",
                "csrw pmpcfg1, zero",
                "csrw pmpcfg2, zero",
                "csrw pmpcfg3, zero",
                "csrw pmpaddr0, {a0}",
                "csrw pmpaddr1, {a1}",
                "csrw pmpaddr2, {a2}",
                "csrw pmpaddr3, {a3}",
                "csrw pmpaddr4, {a4}",
                "csrw pmpaddr5, {a5}",
                "csrw pmpaddr6, {a6}",
                "csrw pmpaddr7, {a7}",
                "csrw pmpaddr8, zero",
                "csrw pmpaddr9, zero",
                "csrw pmpaddr10, zero",
                "csrw pmpaddr11, zero",
                "csrw pmpaddr12, zero",
                "csrw pmpaddr13, zero",
                "csrw pmpaddr14, zero",
                "csrw pmpaddr15, {a15}",
                "csrw pmpcfg0, {c0}",
                "csrw pmpcfg1, {c1}",
                "csrw pmpcfg2, zero",
                "csrw pmpcfg3, {c3}",
                "fence rw, rw",
                a0 = in(reg) self.address[0],
                a1 = in(reg) self.address[1],
                a2 = in(reg) self.address[2],
                a3 = in(reg) self.address[3],
                a4 = in(reg) self.address[4],
                a5 = in(reg) self.address[5],
                a6 = in(reg) self.address[6],
                a7 = in(reg) self.address[7],
                a15 = in(reg) DENY_ALL_ADDR,
                c0 = in(reg) self.config[0],
                c1 = in(reg) self.config[1],
                c3 = in(reg) DENY_ALL_CFG3,
                options(nostack)
            );
            core::arch::asm!("csrr {}, pmpaddr0", out(reg) observed_addr[0], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr1", out(reg) observed_addr[1], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr2", out(reg) observed_addr[2], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr3", out(reg) observed_addr[3], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr4", out(reg) observed_addr[4], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr5", out(reg) observed_addr[5], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr6", out(reg) observed_addr[6], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr7", out(reg) observed_addr[7], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr8", out(reg) observed_addr[8], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr9", out(reg) observed_addr[9], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr10", out(reg) observed_addr[10], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr11", out(reg) observed_addr[11], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr12", out(reg) observed_addr[12], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr13", out(reg) observed_addr[13], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr14", out(reg) observed_addr[14], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr15", out(reg) observed_addr[15], options(nostack));
            core::arch::asm!("csrr {}, pmpcfg0", out(reg) observed_cfg[0], options(nostack));
            core::arch::asm!("csrr {}, pmpcfg1", out(reg) observed_cfg[1], options(nostack));
            core::arch::asm!("csrr {}, pmpcfg2", out(reg) observed_cfg[2], options(nostack));
            core::arch::asm!("csrr {}, pmpcfg3", out(reg) observed_cfg[3], options(nostack));
        }
        observed_addr[..REGIONS] == self.address
            && observed_addr[REGIONS..15] == [0; 15 - REGIONS]
            && observed_addr[15] == DENY_ALL_ADDR
            && observed_cfg[0] == self.config[0]
            && observed_cfg[1] == self.config[1]
            && observed_cfg[2] == 0
            && observed_cfg[3] == DENY_ALL_CFG3
    }

    /// Switch dynamic process entries `0..7` while keeping entry 15 deny-all active.
    /// When `verify` is true (first activation of a new revision), reads back CSRs
    /// to confirm hardware acceptance and intact catch-all protection.
    ///
    /// # Safety
    /// Local hart M-mode, IRQs masked, `activate_initial` already succeeded at boot.
    #[inline(always)]
    pub unsafe fn activate(&self, verify: bool) -> bool {
        unsafe {
            core::arch::asm!(
                "csrw pmpcfg0, zero",
                "csrw pmpcfg1, zero",
                "csrw pmpaddr0, {a0}",
                "csrw pmpaddr1, {a1}",
                "csrw pmpaddr2, {a2}",
                "csrw pmpaddr3, {a3}",
                "csrw pmpaddr4, {a4}",
                "csrw pmpaddr5, {a5}",
                "csrw pmpaddr6, {a6}",
                "csrw pmpaddr7, {a7}",
                "csrw pmpcfg0, {c0}",
                "csrw pmpcfg1, {c1}",
                a0 = in(reg) self.address[0],
                a1 = in(reg) self.address[1],
                a2 = in(reg) self.address[2],
                a3 = in(reg) self.address[3],
                a4 = in(reg) self.address[4],
                a5 = in(reg) self.address[5],
                a6 = in(reg) self.address[6],
                a7 = in(reg) self.address[7],
                c0 = in(reg) self.config[0],
                c1 = in(reg) self.config[1],
                options(nostack)
            );
            if !verify {
                return true;
            }
            let mut observed_addr = [0usize; REGIONS];
            let (mut c0, mut c1, mut a15, mut c3): (usize, usize, usize, usize);
            core::arch::asm!("csrr {}, pmpaddr0", out(reg) observed_addr[0], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr1", out(reg) observed_addr[1], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr2", out(reg) observed_addr[2], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr3", out(reg) observed_addr[3], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr4", out(reg) observed_addr[4], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr5", out(reg) observed_addr[5], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr6", out(reg) observed_addr[6], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr7", out(reg) observed_addr[7], options(nostack));
            core::arch::asm!("csrr {}, pmpcfg0", out(reg) c0, options(nostack));
            core::arch::asm!("csrr {}, pmpcfg1", out(reg) c1, options(nostack));
            core::arch::asm!("csrr {}, pmpaddr15", out(reg) a15, options(nostack));
            core::arch::asm!("csrr {}, pmpcfg3", out(reg) c3, options(nostack));
            observed_addr == self.address
                && c0 == self.config[0]
                && c1 == self.config[1]
                && a15 == DENY_ALL_ADDR
                && c3 == DENY_ALL_CFG3
        }
    }
}
