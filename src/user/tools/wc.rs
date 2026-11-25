#![no_std]
#![no_main]

use vela_abi::syscall as abi;
use vela_user::{Buffer, TextBuf, file::fs_read_bulk, io, sys};

vela_user::entry!(main);

struct Counts {
    lines: u32,
    words: u32,
    bytes: u32,
    in_word: bool,
}

impl Counts {
    const fn new() -> Self {
        Self {
            lines: 0,
            words: 0,
            bytes: 0,
            in_word: false,
        }
    }

    fn feed(&mut self, data: &[u8]) {
        for &b in data {
            self.bytes = self.bytes.saturating_add(1);
            if b == b'\n' {
                self.lines = self.lines.saturating_add(1);
            }
            if b.is_ascii_whitespace() {
                self.in_word = false;
            } else if !self.in_word {
                self.in_word = true;
                self.words = self.words.saturating_add(1);
            }
        }
    }
}

fn main(fs: u32, arg: u32, len: u32, stdin: u32, stdout: u32, _a5: u32) -> ! {
    io::init(stdin, stdout);
    let mut counts = Counts::new();
    let ok = if len == 0 {
        if stdin == 0 {
            io::ewrite(b"usage: wc [</path>]\n");
            false
        } else {
            let mut buf = [0u8; abi::IPC_BYTES];
            while let Ok(n) = io::read(&mut buf) {
                if n == 0 {
                    break;
                }
                counts.feed(&buf[..n]);
            }
            true
        }
    } else if fs == 0 || arg == 0 || len as usize > abi::SPAWN_ARG_MAX {
        io::ewrite(b"usage: wc [</path>]\n");
        false
    } else {
        // SAFETY: SPAWN_IMAGE placed this read-only argument above our initial SP.
        let path = unsafe { core::slice::from_raw_parts(arg as *const u8, len as usize) };
        if !path.starts_with(b"/") {
            io::ewrite(b"usage: wc [</path>]\n");
            false
        } else {
            let mut response = Buffer::new();
            let mut chunk = [0u8; 1024];
            let mut offset = 0u32;
            let mut success = true;
            loop {
                match fs_read_bulk(fs, path, offset, &mut chunk, &mut response) {
                    Ok(0) => break,
                    Ok(n) => {
                        counts.feed(&chunk[..n]);
                        offset = offset.saturating_add(n as u32);
                    }
                    Err(_) => {
                        if response.len() > 1 {
                            io::ewrite(&response.as_slice()[1..]);
                        }
                        success = false;
                        break;
                    }
                }
            }
            success
        }
    };
    if ok {
        let mut out = TextBuf::<64>::new();
        out.decimal(counts.lines);
        out.push(b' ');
        out.decimal(counts.words);
        out.push(b' ');
        out.decimal(counts.bytes);
        out.push(b'\n');
        let _ = io::write(out.as_slice());
    }
    io::close_stdout();
    sys::process_exit();
}
