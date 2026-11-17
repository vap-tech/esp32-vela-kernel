//! Two task instances run the same position-fixed user image, with private
//! stacks and role argument. No yields. All helpers inline into .user.text;
//! only user-section constants may be referenced. FP and heap are not used.
use crate::syscall;
#[unsafe(link_section = ".user.rodata")]
static INIT: [u8; 18] = *b"[task] init: tick ";
#[unsafe(link_section = ".user.rodata")]
static WORKER: [u8; 20] = *b"[task] worker: tick ";
#[inline(always)]
fn call(number: u32, a0: u32, a1: u32) -> u32 {
    let result;
    // SAFETY: ABI syscall only, no privileged operation in U-mode; kernel saves
    // every register. No nomem: write/stats can access the supplied user buffer.
    unsafe {
        core::arch::asm!("ecall", in("a7") number, inlateout("a0") a0 => result, in("a1") a1);
    }
    result
}
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn vela_user_entry(role: u32) -> ! {
    let mut last = 0u32;
    let mut iteration = 0u32;
    loop {
        // Pure user computation: timer may preempt anywhere here. No yield.
        let mut busy = 250_000u32;
        while busy != 0 {
            // Opaque counter prevents LLVM deleting the busy loop.
            unsafe {
                core::arch::asm!("addi {0}, {0}, -1", inout(reg) busy, options(nomem, nostack));
            }
        }
        let now = call(syscall::TICKS, 0, 0);
        if now.wrapping_sub(last) < 100 {
            continue;
        }
        last = now;
        iteration = iteration.wrapping_add(1);
        let prefix = if role == 0 {
            INIT.as_ptr()
        } else {
            WORKER.as_ptr()
        };
        let prefix_len = if role == 0 { 18 } else { 20 };
        let mut buffer = core::mem::MaybeUninit::<[u8; 32]>::uninit();
        let buffer = buffer.as_mut_ptr().cast::<u8>();
        // SAFETY: lengths bounded by fixed prefix and 10 decimal digits + LF.
        // Pointer writes avoid panic/formatting helpers outside the user image.
        unsafe {
            for i in 0..prefix_len {
                buffer.add(i).write_volatile(prefix.add(i).read_volatile());
            }
            let mut value = iteration;
            let mut digits = [0u8; 10];
            let mut count = 0;
            loop {
                digits
                    .as_mut_ptr()
                    .add(count)
                    .write_volatile(b'0' + (value % 10) as u8);
                count += 1;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            for i in 0..count {
                buffer
                    .add(prefix_len + i)
                    .write_volatile(digits.as_ptr().add(count - 1 - i).read_volatile());
            }
            buffer.add(prefix_len + count).write_volatile(b'\n');
            call(
                syscall::WRITE,
                buffer as u32,
                (prefix_len + count + 1) as u32,
            );
        }
    }
}

/// Deliberately access an address supplied by the kernel smoke-test command.
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn vela_user_probe(address: usize) -> ! {
    unsafe {
        core::arch::asm!("lw t0, 0(a0)", in("a0") address, out("t0") _, options(nostack));
    }
    call(syscall::EXIT, 0, 0); // Reached only if the attempted read was permitted.
    loop {
        core::hint::spin_loop();
    }
}

#[unsafe(link_section = ".user.rodata")]
static ABI_OK: [u8; 43] = *b"[abi] write/stats/badptr/yield OK; exiting\n";
#[unsafe(link_section = ".user.rodata")]
static ABI_FAIL: [u8; 12] = *b"[abi] FAIL!\n";
/// Optional U-mode smoke path, selected via console 'a'. Default tasks never yield.
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn vela_user_abi_probe() -> ! {
    let mut snapshot = core::mem::MaybeUninit::<[u8; syscall::STATS_BYTES]>::uninit();
    let ptr = snapshot.as_mut_ptr().cast::<u8>();
    let mut ok = call(syscall::STATS, ptr as u32, syscall::STATS_BYTES as u32) == 0;
    // SAFETY: a successful STATS initializes all 44 bytes of this private stack buffer.
    if ok {
        unsafe {
            ok &= ptr.add(8).read_volatile() == 1;
        }
    }
    ok &= call(syscall::WRITE, 0x4ff40000, 4) == syscall::EFAULT;
    ok &= call(0xffff, 0, 0) == syscall::ENOSYS;
    ok &= call(syscall::YIELD, 0, 0) == 0;
    if ok {
        call(syscall::WRITE, ABI_OK.as_ptr() as u32, ABI_OK.len() as u32);
    } else {
        call(
            syscall::WRITE,
            ABI_FAIL.as_ptr() as u32,
            ABI_FAIL.len() as u32,
        );
    }
    call(syscall::EXIT, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}
