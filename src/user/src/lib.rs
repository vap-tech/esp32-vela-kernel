#![no_std]
//! Standalone U-mode userspace support library (`vela-user`).
//! Provides thin syscall wrappers, bounded IPC message buffers, formatting
//! helpers without `core::fmt` bloat, an explicit bump arena, and userspace
//! sensor/protocol helpers (`ina226`, `sdcard`, `i2c`, `kbd`).

#[cfg(test)]
extern crate std;

pub mod arena;
pub mod boot_cfg;
pub mod ext4;
pub mod fbcon;
pub mod file;
pub mod font;
pub mod i2c;
pub mod ina226;
pub mod io;
pub mod kbd;
pub mod mount;
pub mod sdcard;
pub mod sys;
pub mod term;

use vela_abi::syscall as abi;

pub struct Buffer {
    bytes: [u8; abi::IPC_BYTES],
    len: usize,
    pub reply: u32,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    pub const fn new() -> Self {
        Self {
            bytes: [0; abi::IPC_BYTES],
            len: 0,
            reply: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn set_len(&mut self, len: usize) {
        self.len = len.min(abi::IPC_BYTES);
    }

    pub fn pop(&mut self) -> bool {
        if self.len == 0 {
            false
        } else {
            self.len -= 1;
            true
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    pub fn ptr(&self) -> *const u8 {
        self.bytes.as_ptr()
    }

    pub fn mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }

    pub fn get(&self, index: usize) -> u8 {
        if index < self.len {
            self.bytes[index]
        } else {
            0
        }
    }

    pub fn push(&mut self, byte: u8) {
        if self.len < abi::IPC_BYTES {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    #[inline(never)]
    pub fn append(&mut self, slice: &[u8]) {
        for &byte in slice {
            self.push(byte);
        }
    }

    pub fn starts_with(&self, prefix: &[u8]) -> bool {
        self.as_slice().starts_with(prefix)
    }

    pub fn eq_at(&self, offset: usize, expected: &[u8]) -> bool {
        self.len >= offset && &self.bytes[offset..self.len] == expected
    }

    #[inline(never)]
    pub fn word(&self, offset: usize) -> u32 {
        word_le(self.as_slice(), offset)
    }

    #[inline(never)]
    pub fn decimal(&mut self, mut number: u32) {
        let mut digits = [0u8; 10];
        let mut count = 0;
        loop {
            digits[count] = b'0' + (number % 10) as u8;
            count += 1;
            number /= 10;
            if number == 0 {
                break;
            }
        }
        while count > 0 {
            count -= 1;
            self.push(digits[count]);
        }
    }

    pub fn hundredths(&mut self, number: u32) {
        self.decimal(number / 100);
        self.push(b'.');
        self.push(b'0' + ((number / 10) % 10) as u8);
        self.push(b'0' + (number % 10) as u8);
    }

    #[inline(never)]
    pub fn decimal64(&mut self, mut low: u32, mut high: u32) {
        let mut digits = [0u8; 20];
        let mut count = 0;
        loop {
            let (next_low, next_high, remainder) = div64(low, high, 10);
            digits[count] = b'0' + remainder as u8;
            count += 1;
            low = next_low;
            high = next_high;
            if low == 0 && high == 0 {
                break;
            }
        }
        while count > 0 {
            count -= 1;
            self.push(digits[count]);
        }
    }

    pub fn receive(&mut self, handle: u32) -> bool {
        let (result, reply) = sys::call6(
            abi::RECV,
            handle,
            self.mut_ptr() as u32,
            abi::IPC_BYTES as u32,
            0,
            0,
            0,
        );
        self.reply = reply;
        if result > abi::IPC_BYTES as u32 {
            return false;
        }
        self.len = result as usize;
        true
    }

    pub fn reply_and_receive(
        &mut self,
        reply_token: u32,
        response: &Buffer,
        recv_handle: u32,
    ) -> bool {
        let (result, next_reply) = sys::call6(
            abi::IPC_REPLY_RECV,
            reply_token,
            response.ptr() as u32,
            response.len() as u32,
            recv_handle,
            self.mut_ptr() as u32,
            abi::IPC_BYTES as u32,
        );
        self.reply = next_reply;
        if result > abi::IPC_BYTES as u32 {
            return false;
        }
        self.len = result as usize;
        true
    }

    pub fn send(&self, handle: u32) -> bool {
        loop {
            let result = sys::call(abi::SEND, handle, self.ptr() as u32, self.len as u32, 0);
            if result == 0 {
                return true;
            }
            if result != abi::EAGAIN {
                return false;
            }
            sys::sleep(); // explicit service backpressure, no CPU spin on full queue
        }
    }
}

pub fn div64(low: u32, high: u32, divisor: u32) -> (u32, u32, u32) {
    let (mut qlow, mut qhigh, mut remainder) = (0, 0, 0);
    for index in (0..64).rev() {
        let bit = if index >= 32 {
            (high >> (index - 32)) & 1
        } else {
            (low >> index) & 1
        };
        remainder = (remainder << 1) | bit;
        if remainder >= divisor {
            remainder -= divisor;
            if index >= 32 {
                qhigh |= 1 << (index - 32);
            } else {
                qlow |= 1 << index;
            }
        }
    }
    (qlow, qhigh, remainder)
}

#[inline(never)]
pub fn word_le(bytes: &[u8], offset: usize) -> u32 {
    let mut value = 0u32;
    let mut i = 0;
    while i < 4 {
        if let Some(&b) = bytes.get(offset + i) {
            value |= u32::from(b) << (i * 8);
        }
        i += 1;
    }
    value
}

pub fn dword_le(bytes: &[u8], offset: usize) -> (u32, u32) {
    (word_le(bytes, offset), word_le(bytes, offset + 4))
}

pub struct TextBuf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> TextBuf<N> {
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    pub fn push(&mut self, byte: u8) {
        if self.len < N {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    #[inline(never)]
    pub fn append(&mut self, slice: &[u8]) {
        for &b in slice {
            self.push(b);
        }
    }

    #[inline(never)]
    pub fn decimal(&mut self, mut number: u32) {
        let mut digits = [0u8; 10];
        let mut count = 0;
        loop {
            digits[count] = b'0' + (number % 10) as u8;
            count += 1;
            number /= 10;
            if number == 0 {
                break;
            }
        }
        while count > 0 {
            count -= 1;
            self.push(digits[count]);
        }
    }

    pub fn signed_decimal(&mut self, value: i32) {
        if value < 0 {
            self.push(b'-');
        }
        self.decimal(value.unsigned_abs());
    }

    pub fn hex8(&mut self, value: u8) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        self.append(b"0x");
        self.push(HEX[usize::from(value >> 4)]);
        self.push(HEX[usize::from(value & 0xf)]);
    }

    pub fn hex16(&mut self, value: u16) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        self.append(b"0x");
        for shift in [12, 8, 4, 0] {
            self.push(HEX[((value >> shift) & 0xf) as usize]);
        }
    }

    pub fn hex32(&mut self, value: u32) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        self.append(b"0x");
        for shift in [28, 24, 20, 16, 12, 8, 4, 0] {
            self.push(HEX[((value >> shift) & 0xf) as usize]);
        }
    }

    pub fn hundredths(&mut self, number: u32) {
        self.decimal(number / 100);
        self.push(b'.');
        self.push(b'0' + ((number / 10) % 10) as u8);
        self.push(b'0' + (number % 10) as u8);
    }

    #[inline(never)]
    pub fn decimal64(&mut self, mut low: u32, mut high: u32) {
        let mut digits = [0u8; 20];
        let mut count = 0;
        loop {
            let (next_low, next_high, remainder) = div64(low, high, 10);
            digits[count] = b'0' + remainder as u8;
            count += 1;
            low = next_low;
            high = next_high;
            if low == 0 && high == 0 {
                break;
            }
        }
        while count > 0 {
            count -= 1;
            self.push(digits[count]);
        }
    }
}
