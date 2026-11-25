//! Unified capability-based `stdin` / `stdout` stream helpers for Vela tools.
//!
//! When a program is launched by `shell`:
//! - `stdin == 0`: no input stream attached (`io::read` returns `Ok(0)`).
//! - `stdin != 0`: `CAP_RECEIVE` stream endpoint (pipe read end or terminal input).
//! - `stdout == 0`: default console output via `sys::write`.
//! - `stdout != 0`: `CAP_SEND` stream endpoint (pipe write end or shell redirection sink).
//! - `io::ewrite`: always writes diagnostics directly to the console (`stderr`),
//!   so errors never pollute piped or redirected `stdout`.

use core::sync::atomic::{AtomicU32, Ordering};
use vela_abi::syscall as abi;

use crate::sys;

static STDIN: AtomicU32 = AtomicU32::new(0);
static STDOUT: AtomicU32 = AtomicU32::new(0);

pub fn init(stdin_handle: u32, stdout_handle: u32) {
    STDIN.store(stdin_handle, Ordering::Relaxed);
    STDOUT.store(stdout_handle, Ordering::Relaxed);
}

pub fn stdin() -> u32 {
    STDIN.load(Ordering::Relaxed)
}

pub fn stdout() -> u32 {
    STDOUT.load(Ordering::Relaxed)
}

/// Send raw bytes to `handle` (`CAP_SEND`), yielding on FIFO backpressure (`-EAGAIN`).
pub fn send_stream(handle: u32, bytes: &[u8]) -> Result<(), ()> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let end = (offset + abi::IPC_BYTES).min(bytes.len());
        let chunk = &bytes[offset..end];
        let mut spins = 0u32;
        loop {
            let rc = sys::call(
                abi::SEND,
                handle,
                chunk.as_ptr() as u32,
                chunk.len() as u32,
                0,
            );
            if rc == 0 {
                break;
            }
            if rc == abi::EAGAIN {
                spins = spins.wrapping_add(1);
                if spins < 8 {
                    sys::call(abi::YIELD, 0, 0, 0, 0);
                } else {
                    sys::sleep_ticks(1);
                }
                continue;
            }
            return Err(());
        }
        offset = end;
    }
    Ok(())
}

/// Write bytes to `stdout` (IPC stream if attached, otherwise kernel console).
/// Returns `Err(())` if a piped/redirected `stdout` endpoint was closed by its reader.
#[inline(never)]
pub fn write(bytes: &[u8]) -> Result<(), ()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let handle = stdout();
    if handle == 0 {
        sys::write(bytes);
        Ok(())
    } else {
        send_stream(handle, bytes)
    }
}

/// Write error/diagnostic bytes directly to the console (never swallowed by pipes/redirection).
#[inline(never)]
pub fn ewrite(bytes: &[u8]) {
    sys::write(bytes);
}

/// Read the next chunk of bytes from `stdin` into `buf` (at most `abi::IPC_BYTES`).
/// Returns `Ok(0)` on EOF or if no `stdin` handle is attached, `Ok(n)` on data,
/// or `Err(())` if the stream was closed/revoked.
#[inline(never)]
pub fn read(buf: &mut [u8]) -> Result<usize, ()> {
    let handle = stdin();
    if handle == 0 || buf.is_empty() {
        return Ok(0);
    }
    let cap = buf.len().min(abi::IPC_BYTES);
    let (res, _) = sys::call6(
        abi::RECV,
        handle,
        buf.as_mut_ptr() as u32,
        cap as u32,
        0,
        0,
        0,
    );
    if (res as i32) < 0 || res as usize > cap {
        return Err(());
    }
    Ok(res as usize)
}

/// Explicitly close the process's `stdout` handle (if any) before exiting.
pub fn close_stdout() {
    let handle = STDOUT.swap(0, Ordering::Relaxed);
    if handle != 0 {
        let _ = sys::endpoint_close(handle);
    }
}
