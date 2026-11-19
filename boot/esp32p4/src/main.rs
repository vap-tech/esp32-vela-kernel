#![no_std]
#![no_main]

#[path = "../../../arch/riscv32/trap.rs"]
mod arch;
#[path = "../../../bsp/m5stack-tab5/mod.rs"]
mod board;
#[path = "../../../pac/esp32p4-v13/mod.rs"]
mod pac;
#[path = "../../../arch/riscv32/pmp.rs"]
mod pmp;
#[path = "../../../abi/sentinel.rs"]
mod sentinel_wire;
#[path = "../../../soc/esp32p4-v13/mod.rs"]
mod soc;
#[allow(dead_code)]
#[path = "../../../abi/syscall.rs"]
mod syscall;
#[path = "../../../userspace/services.rs"]
mod users;

mod boot;
mod calls;
mod dispatch;
mod display;
mod dma;
mod runtime;
mod sentinel;
mod trap;
mod usermem;

core::arch::global_asm!(include_str!("../entry.S"));
