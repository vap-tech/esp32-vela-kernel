use vela_abi::syscall as abi;

pub fn call6(number: u32, a0: u32, a1: u32, a2: u32, a3: u32, a4: u32, a5: u32) -> (u32, u32) {
    #[cfg(target_arch = "riscv32")]
    {
        let (result, second);
        // SAFETY: U-mode ecall ABI; integer registers preserved except a0/a1 return pair.
        unsafe {
            core::arch::asm!(
                "ecall",
                in("a7") number,
                inlateout("a0") a0 => result,
                inlateout("a1") a1 => second,
                in("a2") a2,
                in("a3") a3,
                in("a4") a4,
                in("a5") a5,
            );
        }
        (result, second)
    }
    #[cfg(not(target_arch = "riscv32"))]
    {
        let _ = (number, a0, a1, a2, a3, a4, a5);
        (abi::ENOSYS, 0)
    }
}

pub fn call(number: u32, a0: u32, a1: u32, a2: u32, a3: u32) -> u32 {
    call6(number, a0, a1, a2, a3, 0, 0).0
}

#[inline(never)]
pub fn write(bytes: &[u8]) {
    let mut offset = 0;
    while offset < bytes.len() {
        let chunk = (bytes.len() - offset).min(128);
        let ptr = unsafe { bytes.as_ptr().add(offset) };
        call(abi::WRITE, ptr as u32, chunk as u32, 0, 0);
        offset += chunk;
    }
}

pub fn sleep_ticks(ticks: u32) {
    call(abi::SLEEP, ticks, 0, 0, 0);
}

pub fn sleep() {
    sleep_ticks(1);
}

/// Whether the screen is blanked (backlight off). The keyboard service uses it to swallow
/// the key that wakes the display.
pub fn display_blanked() -> bool {
    display_flags() & abi::DISPLAY_FLAG_BLANKED != 0
}

pub fn display_flags() -> u32 {
    let mut flags = [0u8; abi::STATS_DISPLAY_BYTES];
    let rc = call(
        abi::STATS,
        flags.as_mut_ptr() as u32,
        abi::STATS_DISPLAY_BYTES as u32,
        abi::STATS_DISPLAY,
        0,
    );
    if rc == 0 {
        u32::from_le_bytes(flags)
    } else {
        0
    }
}

pub fn event_wait(handle: u32, timeout_ticks: u32) -> u32 {
    call(abi::EVENT_WAIT, handle, timeout_ticks, 0, 0)
}

pub fn irq_ack(handle: u32) -> u32 {
    call(abi::IRQ_ACK, handle, 0, 0, 0)
}

pub fn endpoint_create() -> Result<u32, i32> {
    let rc = call(abi::ENDPOINT_CREATE, 0, 0, 0, 0) as i32;
    if rc <= 0 { Err(rc) } else { Ok(rc as u32) }
}

pub fn endpoint_close(handle: u32) -> i32 {
    call(abi::ENDPOINT_CLOSE, handle, 0, 0, 0) as i32
}

pub fn ipc_call_bulk(endpoint: u32, desc: &abi::IpcBulkCall) -> i32 {
    call(abi::IPC_CALL_BULK, endpoint, desc as *const _ as u32, 0, 0) as i32
}

pub fn ipc_call_cap(endpoint: u32, desc: &abi::IpcCapCall) -> i32 {
    call(abi::IPC_CALL_CAP, endpoint, desc as *const _ as u32, 0, 0) as i32
}

/// Query `(caller_rights, has_attached_cap)` for an in-flight RPC call without
/// allocating a capability slot.
pub fn ipc_call_rights(token: u32) -> Result<(u32, bool), i32> {
    let (rights, has_cap) = call6(
        abi::IPC_ACCEPT_CAP,
        token,
        abi::IPC_ACCEPT_CAP_QUERY,
        0,
        0,
        0,
        0,
    );
    if (rights as i32) < 0 {
        Err(rights as i32)
    } else {
        Ok((rights, has_cap != 0))
    }
}

/// Accept the capability attached to `token`, returning `(installed_handle, caller_rights)`.
pub fn ipc_accept_cap(token: u32) -> Result<(u32, u32), i32> {
    let (handle, rights) = call6(abi::IPC_ACCEPT_CAP, token, 0, 0, 0, 0, 0);
    if (handle as i32) <= 0 {
        Err(handle as i32)
    } else {
        Ok((handle, rights))
    }
}

pub fn ipc_bulk_write(token: u32, client_offset: usize, src: &[u8]) -> Result<usize, i32> {
    if src.is_empty() {
        return Ok(0);
    }
    let rc = call6(
        abi::IPC_BULK_IO,
        token,
        client_offset as u32,
        src.as_ptr() as u32,
        src.len() as u32,
        abi::IPC_BULK_DIR_WRITE,
        0,
    )
    .0 as i32;
    if rc < 0 { Err(rc) } else { Ok(rc as usize) }
}

pub fn ipc_bulk_read(token: u32, client_offset: usize, dst: &mut [u8]) -> Result<usize, i32> {
    if dst.is_empty() {
        return Ok(0);
    }
    let rc = call6(
        abi::IPC_BULK_IO,
        token,
        client_offset as u32,
        dst.as_mut_ptr() as u32,
        dst.len() as u32,
        abi::IPC_BULK_DIR_READ,
        0,
    )
    .0 as i32;
    if rc < 0 { Err(rc) } else { Ok(rc as usize) }
}

pub fn irq_bind(irq_ctl_handle: u32, slot: u32) -> u32 {
    call(abi::IRQ_BIND, irq_ctl_handle, slot, 0, 0)
}

pub fn irq_unbind(event_handle: u32) -> u32 {
    call(abi::IRQ_UNBIND, event_handle, 0, 0, 0)
}

pub fn i2c_transfer(handle: u32, addr: u8, tx: &[u8], rx: &mut [u8]) -> Result<(), i32> {
    let tx_ptr = if tx.is_empty() { 0 } else { tx.as_ptr() as u32 };
    let rx_ptr = if rx.is_empty() {
        0
    } else {
        rx.as_mut_ptr() as u32
    };
    let rc = call6(
        abi::I2C_TRANSFER,
        handle,
        u32::from(addr),
        tx_ptr,
        tx.len() as u32,
        rx_ptr,
        rx.len() as u32,
    )
    .0 as i32;
    if rc == 0 { Ok(()) } else { Err(rc) }
}

/// Read console output from byte `cursor` on; returns the bytes copied and the next cursor.
pub fn console_log(handle: u32, buffer: &mut [u8], cursor: u32) -> (usize, u32) {
    let len = buffer.len().min(128) as u32;
    let (count, next) = call6(
        abi::CONSOLE_LOG,
        handle,
        buffer.as_mut_ptr() as u32,
        len,
        cursor,
        0,
        0,
    );
    if (count as i32) < 0 {
        (0, cursor)
    } else {
        (count as usize, next)
    }
}

pub fn exit() -> ! {
    call(abi::EXIT, 0, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

pub fn process_exit() -> ! {
    call(abi::PROCESS_EXIT, 0, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(link_section = ".text.entry")]
        #[unsafe(no_mangle)]
        pub extern "C" fn _start(a0: u32, a1: u32, a2: u32, a3: u32, a4: u32, a5: u32) -> ! {
            $main(a0, a1, a2, a3, a4, a5)
        }

        #[panic_handler]
        fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
            $crate::sys::process_exit()
        }
    };
}

pub fn rdcycle64() -> u64 {
    #[cfg(target_arch = "riscv32")]
    {
        // SAFETY: U-mode read-only shadow CSRs `cycle`/`cycleh` enabled via `mcounteren.CY = 1`.
        unsafe {
            loop {
                let hi1: u32;
                let lo: u32;
                let hi2: u32;
                core::arch::asm!(
                    "rdcycleh {0}",
                    "rdcycle {1}",
                    "rdcycleh {2}",
                    out(reg) hi1,
                    out(reg) lo,
                    out(reg) hi2,
                    options(nomem, nostack)
                );
                if hi1 == hi2 {
                    return (u64::from(hi1) << 32) | u64::from(lo);
                }
            }
        }
    }
    #[cfg(not(target_arch = "riscv32"))]
    {
        0
    }
}
