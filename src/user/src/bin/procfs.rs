#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{Buffer, div64, sys};

vela_user::entry!(main);

/// Tiny virtual-file interface: Read request carries a path; response byte 0 is
/// status (0 success, 1 ENOENT, 2 invalid request), followed by file contents.
fn read_node(request: &Buffer, response: &mut Buffer) {
    response.clear();
    if request.get(0) != 1 {
        response.push(2);
        response.append(b"procfs: bad request\n");
        return;
    }
    let load = request.eq_at(1, b"/proc/loadavg");
    let uptime = request.eq_at(1, b"/proc/uptime");
    let interrupts = request.eq_at(1, b"/proc/interrupts");
    if !load && !uptime && !interrupts {
        response.push(1);
        response.append(b"procfs: no such file\n");
        return;
    }
    let mut snapshot = Buffer::new();
    let size = if interrupts {
        abi::STATS_IRQ_BYTES
    } else {
        abi::STATS_BYTES
    };
    if sys::call(abi::STATS, snapshot.mut_ptr() as u32, size as u32, 0, 0) != 0 {
        response.push(2);
        response.append(b"service unavailable\n");
        return;
    }
    snapshot.set_len(size);
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
        response.append(b"systimer0 ");
        response.decimal64(snapshot.word(16), snapshot.word(20));
        response.push(b'\n');
        response.append(b"usb_serial_jtag_rx ");
        response.decimal64(snapshot.word(44), snapshot.word(48));
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

fn main(request_handle: u32, reply_handle: u32, _a2: u32, _a3: u32) -> ! {
    loop {
        let mut request = Buffer::new();
        if !request.receive(request_handle) {
            sys::exit();
        }
        let mut response = Buffer::new();
        read_node(&request, &mut response);
        if request.reply != 0 {
            // A timed-out client invalidates its one-shot reply token. Keep serving.
            let _ = sys::call(
                abi::IPC_REPLY,
                request.reply,
                response.ptr() as u32,
                response.len() as u32,
                0,
            );
        } else if !response.send(reply_handle) {
            sys::exit();
        }
    }
}
