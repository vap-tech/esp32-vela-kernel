//! P4 rev1.3: 16 entries, minimum 128B, no NA4. Never lock dynamic mappings.
//! Full deny-all at entry15 protects holes, MMIO and uncached aliases.
use vela_kernel::process::AddressSpace;
#[derive(Clone, Copy)]
pub struct Prepared {
    address: [usize; 16],
    config: [usize; 4],
}
impl Prepared {
    pub fn new(memory: &AddressSpace) -> Result<Self, u32> {
        let mut result = Self {
            address: [0; 16],
            config: [0; 4],
        };
        for (index, mapping) in memory.maps.iter().enumerate() {
            let r = mapping.region;
            if r.size == 0 {
                continue;
            }
            if r.size < 128 || !r.size.is_power_of_two() || r.base % r.size != 0 {
                return Err(22);
            }
            result.address[index] = (r.base >> 2) | ((r.size >> 3) - 1);
            result.config[index / 4] |=
                (0x18 | usize::from(mapping.permissions)) << ((index % 4) * 8);
        }
        result.address[15] = 0x1fff_ffff;
        result.config[3] |= 0x1800_0000;
        Ok(result)
    }
    /// # Safety
    /// Local hart M-mode, IRQs masked, no user execution. All inherited entries
    /// must be unlocked; exact CSR readback rejects unsupported/locked layouts.
    pub unsafe fn activate(&self) -> bool {
        let mut observed_address = [0usize; 16];
        let mut observed_config = [0usize; 4];
        unsafe {
            core::arch::asm!(
                "csrw pmpcfg0,zero",
                "csrw pmpcfg1,zero",
                "csrw pmpcfg2,zero",
                "csrw pmpcfg3,zero",
                options(nostack)
            );
            core::arch::asm!("csrw pmpaddr0, {}", in(reg)self.address[0], options(nostack));
            core::arch::asm!("csrw pmpaddr1, {}", in(reg)self.address[1], options(nostack));
            core::arch::asm!("csrw pmpaddr2, {}", in(reg)self.address[2], options(nostack));
            core::arch::asm!("csrw pmpaddr3, {}", in(reg)self.address[3], options(nostack));
            core::arch::asm!("csrw pmpaddr4, {}", in(reg)self.address[4], options(nostack));
            core::arch::asm!("csrw pmpaddr5, {}", in(reg)self.address[5], options(nostack));
            core::arch::asm!("csrw pmpaddr6, {}", in(reg)self.address[6], options(nostack));
            core::arch::asm!("csrw pmpaddr7, {}", in(reg)self.address[7], options(nostack));
            core::arch::asm!("csrw pmpaddr8, {}", in(reg)self.address[8], options(nostack));
            core::arch::asm!("csrw pmpaddr9, {}", in(reg)self.address[9], options(nostack));
            core::arch::asm!("csrw pmpaddr10, {}", in(reg)self.address[10], options(nostack));
            core::arch::asm!("csrw pmpaddr11, {}", in(reg)self.address[11], options(nostack));
            core::arch::asm!("csrw pmpaddr12, {}", in(reg)self.address[12], options(nostack));
            core::arch::asm!("csrw pmpaddr13, {}", in(reg)self.address[13], options(nostack));
            core::arch::asm!("csrw pmpaddr14, {}", in(reg)self.address[14], options(nostack));
            core::arch::asm!("csrw pmpaddr15, {}", in(reg)self.address[15], options(nostack));
            core::arch::asm!("csrw pmpcfg0, {}", in(reg)self.config[0], options(nostack));
            core::arch::asm!("csrw pmpcfg1, {}", in(reg)self.config[1], options(nostack));
            core::arch::asm!("csrw pmpcfg2, {}", in(reg)self.config[2], options(nostack));
            core::arch::asm!("csrw pmpcfg3, {}", in(reg)self.config[3], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr0", out(reg)observed_address[0], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr1", out(reg)observed_address[1], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr2", out(reg)observed_address[2], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr3", out(reg)observed_address[3], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr4", out(reg)observed_address[4], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr5", out(reg)observed_address[5], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr6", out(reg)observed_address[6], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr7", out(reg)observed_address[7], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr8", out(reg)observed_address[8], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr9", out(reg)observed_address[9], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr10", out(reg)observed_address[10], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr11", out(reg)observed_address[11], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr12", out(reg)observed_address[12], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr13", out(reg)observed_address[13], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr14", out(reg)observed_address[14], options(nostack));
            core::arch::asm!("csrr {}, pmpaddr15", out(reg)observed_address[15], options(nostack));
            core::arch::asm!("csrr {}, pmpcfg0", out(reg)observed_config[0], options(nostack));
            core::arch::asm!("csrr {}, pmpcfg1", out(reg)observed_config[1], options(nostack));
            core::arch::asm!("csrr {}, pmpcfg2", out(reg)observed_config[2], options(nostack));
            core::arch::asm!("csrr {}, pmpcfg3", out(reg)observed_config[3], options(nostack));
            core::arch::asm!("fence rw,rw", "fence.i", options(nostack));
        }
        self.address == observed_address && self.config == observed_config
    }
}
