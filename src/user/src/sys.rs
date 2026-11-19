use vela_abi::syscall as abi;

#[inline(always)]
pub fn call6(number: u32, a0: u32, a1: u32, a2: u32, a3: u32, a4: u32, a5: u32) -> (u32, u32) {
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

#[inline(always)]
pub fn call(number: u32, a0: u32, a1: u32, a2: u32, a3: u32) -> u32 {
    call6(number, a0, a1, a2, a3, 0, 0).0
}

#[inline(always)]
pub fn write(bytes: &[u8]) {
    let mut offset = 0;
    while offset < bytes.len() {
        let chunk = (bytes.len() - offset).min(128);
        let ptr = unsafe { bytes.as_ptr().add(offset) };
        call(abi::WRITE, ptr as u32, chunk as u32, 0, 0);
        offset += chunk;
    }
}

#[inline(always)]
pub fn sleep_ticks(ticks: u32) {
    call(abi::SLEEP, ticks, 0, 0, 0);
}

#[inline(always)]
pub fn sleep() {
    sleep_ticks(1);
}

#[inline(always)]
pub fn exit() -> ! {
    call(abi::EXIT, 0, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}

#[inline(always)]
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

#[inline(always)]
pub fn rdcycle64() -> u64 {
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
