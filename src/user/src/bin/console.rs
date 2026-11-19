#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{Buffer, sys};

vela_user::entry!(main);

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
        }
        if length == 0 && sys::call(abi::EVENT_WAIT, rx_event, 0, 0, 0) != 0 {
            sys::exit();
        }
    }
}
