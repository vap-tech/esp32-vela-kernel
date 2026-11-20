#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{Buffer, sys};

vela_user::entry!(main);

const IDLE_POLL_TICKS: u32 = 1000;

fn main(input_handle: u32, rx_event: u32, _a2: u32, _a3: u32, _a4: u32, _a5: u32) -> ! {
    loop {
        let mut input = Buffer::new();
        let length = sys::call(abi::CONSOLE_READ, rx_event, input.mut_ptr() as u32, 32, 0);
        if length > 32 {
            sys::exit();
        }
        if length != 0 {
            input.set_len(length as usize);
            if !input.send(input_handle) {
                sys::exit();
            }
            continue;
        }
        let wait_rc = sys::event_wait(rx_event, IDLE_POLL_TICKS);
        if wait_rc != 0 && wait_rc != abi::ETIMEDOUT {
            sys::exit();
        }
    }
}
