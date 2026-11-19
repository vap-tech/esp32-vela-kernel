#![no_std]
#![no_main]

pub(crate) use arch::pmp;
pub(crate) use vela_abi::{sentinel as sentinel_wire, syscall};
pub(crate) use vela_user as users;

mod arch;
mod board;
mod boot;
mod context;
mod dispatch;
mod display;
mod dma;
mod pac;
mod runtime;
mod sentinel;
mod soc;
mod trap;
mod usermem;

core::arch::global_asm!(include_str!("../entry.S"));
