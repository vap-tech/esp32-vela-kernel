//! Fixed-address U-mode services. Shared RX image, private stacks. Every helper
//! is inlined or placed in .user.text; literals live only in .user.rodata.
//! No allocation, kernel references, shared writable memory, or core formatting.
use crate::syscall as abi;
#[path = "arena.rs"]
mod arena;
use core::mem::MaybeUninit;
macro_rules! text {
    ($name:ident, $bytes:literal) => {
        #[unsafe(link_section = ".user.rodata")]
        static $name: [u8; $bytes.len()] = *$bytes;
    };
}
text!(PROMPT, b"vela:/$ ");
text!(
    HELP,
    b"help | cat <path> | uptime | run hello | run pic | memtest\n"
);
text!(HELP_CMD, b"help");
text!(RUN_PIC, b"run pic");
text!(RUN_HELLO, b"run hello");
text!(MEMTEST_CMD, b"memtest");
text!(MEM_OK, b"arena: aligned allocations, bounds, release OK\n");
text!(MEM_FAIL, b"arena: FAIL\n");
text!(UPTIME_CMD, b"uptime");
text!(CAT, b"cat ");
text!(LOADAVG, b"/proc/loadavg");
text!(INTERRUPTS, b"/proc/interrupts");
text!(TIMER_IRQ, b"systimer0 ");
text!(UPTIME, b"/proc/uptime");
text!(NOT_FOUND, b"procfs: no such file\n");
text!(BAD_REQUEST, b"procfs: bad request\n");
text!(FAILED, b"service unavailable\n");
text!(LINE_LONG, b"line too long\n");
text!(UNKNOWN, b"unknown command (try help)\n");
text!(ERASE, b"\x08 \x08");
text!(NEWLINE, b"\n");

#[inline(always)]
fn call6(number: u32, a0: u32, a1: u32, a2: u32, a3: u32, a4: u32, a5: u32) -> (u32, u32) {
    let (result, second);
    // SAFETY: syscall boundary; all integer state preserved except a0/a1 results.
    unsafe {
        core::arch::asm!("ecall",in("a7")number,inlateout("a0")a0=>result,
        inlateout("a1")a1=>second,in("a2")a2,in("a3")a3,in("a4")a4,in("a5")a5);
    }
    (result, second)
}
#[inline(always)]
fn call(number: u32, a0: u32, a1: u32, a2: u32, a3: u32) -> u32 {
    call6(number, a0, a1, a2, a3, 0, 0).0
}
#[inline(always)]
fn sleep() {
    call(abi::SLEEP, 1, 0, 0, 0);
}
#[inline(always)]
fn exit() -> ! {
    call(abi::EXIT, 0, 0, 0, 0);
    loop {
        core::hint::spin_loop();
    }
}
#[inline(always)]
fn write(pointer: *const u8, length: usize) {
    call(abi::WRITE, pointer as u32, length as u32, 0, 0);
}
struct Buffer {
    bytes: MaybeUninit<[u8; abi::IPC_BYTES]>,
    len: usize,
    reply: u32,
}
impl Buffer {
    #[inline(always)]
    fn new() -> Self {
        Self {
            bytes: MaybeUninit::uninit(),
            len: 0,
            reply: 0,
        }
    }
    #[inline(always)]
    fn ptr(&self) -> *const u8 {
        self.bytes.as_ptr().cast::<u8>()
    }
    #[inline(always)]
    fn mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr().cast::<u8>()
    }
    #[inline(always)]
    fn push(&mut self, byte: u8) {
        if self.len < abi::IPC_BYTES {
            // SAFETY: bounded destination, len tracks initialized bytes only.
            unsafe {
                self.mut_ptr().add(self.len).write_volatile(byte);
            }
            self.len += 1;
        }
    }
    #[inline(always)]
    fn get(&self, index: usize) -> u8 {
        if index >= self.len {
            return 0;
        }
        // SAFETY: index below initialized length.
        unsafe { self.ptr().add(index).read_volatile() }
    }
    #[inline(always)]
    fn append(&mut self, pointer: *const u8, length: usize) {
        for i in 0..length {
            // SAFETY: callers provide static RX bytes or initialized Buffer data.
            self.push(unsafe { pointer.add(i).read_volatile() });
        }
    }
    #[inline(always)]
    fn matches(&self, offset: usize, pointer: *const u8, length: usize) -> bool {
        if self.len != offset + length {
            return false;
        }
        for i in 0..length {
            // SAFETY: static bytes length supplied by caller, no kernel memory.
            if self.get(offset + i) != unsafe { pointer.add(i).read_volatile() } {
                return false;
            }
        }
        true
    }
    #[inline(always)]
    fn word(&self, offset: usize) -> u32 {
        u32::from(self.get(offset))
            | (u32::from(self.get(offset + 1)) << 8)
            | (u32::from(self.get(offset + 2)) << 16)
            | (u32::from(self.get(offset + 3)) << 24)
    }
    #[inline(always)]
    fn decimal(&mut self, mut number: u32) {
        let mut digits = MaybeUninit::<[u8; 10]>::uninit();
        let ptr = digits.as_mut_ptr().cast::<u8>();
        let mut count = 0;
        loop {
            // SAFETY: u32 decimal representation has at most 10 digits.
            unsafe {
                ptr.add(count).write_volatile(b'0' + (number % 10) as u8);
            }
            count += 1;
            number /= 10;
            if number == 0 {
                break;
            }
        }
        for i in 0..count {
            self.push(unsafe { ptr.add(count - 1 - i).read_volatile() });
        }
    }
    #[inline(always)]
    fn hundredths(&mut self, number: u32) {
        self.decimal(number / 100);
        self.push(b'.');
        self.push(b'0' + ((number / 10) % 10) as u8);
        self.push(b'0' + (number % 10) as u8);
    }
    #[inline(never)]
    #[unsafe(link_section = ".user.text")]
    fn decimal64(&mut self, mut low: u32, mut high: u32) {
        let mut digits = MaybeUninit::<[u8; 20]>::uninit();
        let ptr = digits.as_mut_ptr().cast::<u8>();
        let mut count = 0;
        loop {
            let (next_low, next_high, remainder) = div64(low, high, 10);
            // SAFETY: a 64-bit integer has at most 20 decimal digits.
            unsafe {
                ptr.add(count).write_volatile(b'0' + remainder as u8);
            }
            count += 1;
            low = next_low;
            high = next_high;
            if low == 0 && high == 0 {
                break;
            }
        }
        for i in 0..count {
            self.push(unsafe { ptr.add(count - 1 - i).read_volatile() });
        }
    }
    #[inline(always)]
    fn receive(&mut self, handle: u32) -> bool {
        let (result, reply) = call6(
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
    #[inline(always)]
    fn send(&self, handle: u32) -> bool {
        loop {
            let result = call(abi::SEND, handle, self.ptr() as u32, self.len as u32, 0);
            if result == 0 {
                return true;
            }
            if result != abi::EAGAIN {
                return false;
            }
            sleep(); // explicit service backpressure, no CPU spin on full queue
        }
    }
}
// Software word-pair division avoids importing kernel compiler-runtime routines
// into the PMP-isolated image. Used only for formatting a 64-bit uptime.
#[inline(always)]
fn div64(low: u32, high: u32, divisor: u32) -> (u32, u32, u32) {
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

/// Tiny virtual-file interface: Read request carries a path; response byte0 is
/// status (0 success, 1 ENOENT, 2 invalid request), followed by file contents.
/// Policy, node selection and formatting are exclusively in this user task.
#[inline(always)]
fn read_node(request: &Buffer, response: &mut Buffer) {
    response.len = 0;
    if request.get(0) != 1 {
        response.push(2);
        response.append(BAD_REQUEST.as_ptr(), BAD_REQUEST.len());
        return;
    }
    let load = request.matches(1, LOADAVG.as_ptr(), LOADAVG.len());
    let uptime = request.matches(1, UPTIME.as_ptr(), UPTIME.len());
    let interrupts = request.matches(1, INTERRUPTS.as_ptr(), INTERRUPTS.len());
    if !load && !uptime && !interrupts {
        response.push(1);
        response.append(NOT_FOUND.as_ptr(), NOT_FOUND.len());
        return;
    }
    let mut snapshot = Buffer::new();
    if call(
        abi::STATS,
        snapshot.mut_ptr() as u32,
        abi::STATS_BYTES as u32,
        0,
        0,
    ) != 0
    {
        response.push(2);
        response.append(FAILED.as_ptr(), FAILED.len());
        return;
    }
    snapshot.len = abi::STATS_BYTES;
    response.push(0);
    if load {
        for offset in [32, 36, 40] {
            let hundredths = (snapshot.word(offset) * 100 + 32768) >> 16;
            response.hundredths(hundredths);
            response.push(b' ');
        }
        response.decimal(snapshot.word(4));
        response.push(b'/');
        response.decimal(snapshot.word(0));
        response.push(b' ');
        response.decimal(snapshot.word(12));
        response.push(b'\n');
    } else if interrupts {
        response.append(TIMER_IRQ.as_ptr(), TIMER_IRQ.len());
        response.decimal64(snapshot.word(16), snapshot.word(20));
        response.push(b'\n');
    } else {
        let (low, high, remainder) = div64(snapshot.word(24), snapshot.word(28), 100);
        response.decimal64(low, high);
        response.push(b'.');
        response.push(b'0' + (remainder / 10) as u8);
        response.push(b'0' + (remainder % 10) as u8);
        response.push(b'\n');
    }
}
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn procfs_main(request_handle: u32, reply_handle: u32) -> ! {
    loop {
        let mut request = Buffer::new();
        if !request.receive(request_handle) {
            exit();
        }
        let mut response = Buffer::new();
        read_node(&request, &mut response);
        if request.reply != 0 {
            // A timed-out client invalidates its one-shot reply token. Keep serving.
            let _ = call(
                abi::IPC_REPLY,
                request.reply,
                response.ptr() as u32,
                response.len as u32,
                0,
            );
        } else if !response.send(reply_handle) {
            exit();
        }
    }
}
#[inline(always)]
fn read_file(path: *const u8, length: usize, request_handle: u32, _reply_handle: u32) {
    let mut request = Buffer::new();
    request.push(1);
    request.append(path, length);
    let mut response = Buffer::new();
    let result = call6(
        abi::IPC_CALL,
        request_handle,
        request.ptr() as u32,
        request.len as u32,
        response.mut_ptr() as u32,
        abi::IPC_BYTES as u32,
        100,
    )
    .0;
    if result == 0 || result > abi::IPC_BYTES as u32 {
        write(FAILED.as_ptr(), FAILED.len());
        return;
    }
    response.len = result as usize;
    // Error replies carry userspace diagnostics, never fabricated file data.
    unsafe {
        write(response.ptr().add(1), response.len - 1);
    }
}
#[inline(always)]
fn arena_probe() -> bool {
    let Ok(mut arena) = arena::Arena::new(4096) else {
        return false;
    };
    let layout = core::alloc::Layout::from_size_align(31, 64).unwrap();
    let first = arena.allocate(layout);
    let second = arena.allocate(layout);
    let mut ok = false;
    if let (Some(a), Some(b)) = (first, second) {
        // SAFETY: distinct live allocations, both at least 31 writable bytes.
        unsafe {
            a.as_ptr().write_volatile(0x5a);
            b.as_ptr().write_volatile(0xa5);
            ok = a.as_ptr().read_volatile() == 0x5a && b.as_ptr().read_volatile() == 0xa5;
        }
        ok &= a.as_ptr() as usize % 64 == 0 && b.as_ptr() as usize % 64 == 0;
    }
    ok &= arena
        .allocate(core::alloc::Layout::from_size_align(4096, 1).unwrap())
        .is_none();
    let released = arena.release().is_ok();
    ok && released && arena.allocate(layout).is_none()
}
#[inline(always)]
fn command(line: &Buffer, request_handle: u32, reply_handle: u32) {
    if line.len == 0 {
        return;
    }
    if line.matches(0, HELP_CMD.as_ptr(), HELP_CMD.len()) {
        write(HELP.as_ptr(), HELP.len());
    } else if line.matches(0, RUN_HELLO.as_ptr(), RUN_HELLO.len())
        || line.matches(0, RUN_PIC.as_ptr(), RUN_PIC.len())
    {
        let selector = u32::from(line.len == RUN_PIC.len());
        if (call(abi::SPAWN_HELLO, selector, 0, 0, 0) as i32) < 0 {
            write(FAILED.as_ptr(), FAILED.len());
        }
    } else if line.matches(0, MEMTEST_CMD.as_ptr(), MEMTEST_CMD.len()) {
        if arena_probe() {
            write(MEM_OK.as_ptr(), MEM_OK.len());
        } else {
            write(MEM_FAIL.as_ptr(), MEM_FAIL.len());
        }
    } else if line.matches(0, UPTIME_CMD.as_ptr(), UPTIME_CMD.len()) {
        read_file(UPTIME.as_ptr(), UPTIME.len(), request_handle, reply_handle);
    } else if line.len > CAT.len()
        && line.get(0) == b'c'
        && line.get(1) == b'a'
        && line.get(2) == b't'
        && line.get(3) == b' '
    {
        unsafe {
            read_file(
                line.ptr().add(4),
                line.len - 4,
                request_handle,
                reply_handle,
            );
        }
    } else {
        write(UNKNOWN.as_ptr(), UNKNOWN.len());
    }
}
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn shell_main(reply_handle: u32, request_handle: u32, input_handle: u32) -> ! {
    let mut line = Buffer::new();
    let mut skip_lf = false;
    let mut overflow = false;
    write(PROMPT.as_ptr(), PROMPT.len());
    loop {
        let mut input = Buffer::new();
        if !input.receive(input_handle) {
            exit();
        }
        for i in 0..input.len {
            let byte = input.get(i);
            if byte == b'\n' && skip_lf {
                skip_lf = false;
                continue;
            }
            skip_lf = byte == b'\r';
            if byte == b'\r' || byte == b'\n' {
                write(NEWLINE.as_ptr(), NEWLINE.len());
                if overflow {
                    write(LINE_LONG.as_ptr(), LINE_LONG.len());
                } else {
                    command(&line, request_handle, reply_handle);
                }
                line.len = 0;
                overflow = false;
                write(PROMPT.as_ptr(), PROMPT.len());
            } else if byte == 8 || byte == 127 {
                if line.len != 0 {
                    line.len -= 1;
                    write(ERASE.as_ptr(), ERASE.len());
                }
            } else if (32..127).contains(&byte) {
                if line.len < 80 {
                    line.push(byte);
                    // SAFETY: byte is a valid stack-local value for synchronous write.
                    write(core::ptr::addr_of!(byte), 1);
                } else {
                    overflow = true;
                }
            }
        }
    }
}
#[unsafe(link_section = ".user.text")]
#[unsafe(no_mangle)]
pub extern "C" fn console_main(input_handle: u32, rx_event: u32) -> ! {
    loop {
        let mut input = Buffer::new();
        let length = call(abi::CONSOLE_READ, input.mut_ptr() as u32, 32, 0, 0);
        if length > 32 {
            exit();
        }
        if length != 0 {
            input.len = length as usize;
            if !input.send(input_handle) {
                exit();
            }
        }
        if length == 0 && call(abi::EVENT_WAIT, rx_event, 0, 0, 0) != 0 {
            exit();
        }
    }
}
