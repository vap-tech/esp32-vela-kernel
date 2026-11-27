#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]

#[cfg(not(test))]
use vela_abi::syscall as abi;
#[cfg(not(test))]
use vela_user::{Buffer, TextBuf, arena::Arena, file::fs_read_bulk, io, sys};

#[cfg(not(test))]
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
        self.bytes = self.bytes.saturating_add(data.len() as u32);
        for &b in data {
            if b == b'\n' {
                self.lines = self.lines.saturating_add(1);
            }
            if matches!(b, b' ' | b'\t'..=b'\r') {
                self.in_word = false;
            } else if !self.in_word {
                self.in_word = true;
                self.words = self.words.saturating_add(1);
            }
        }
    }
}

#[cfg(not(test))]
fn main(fs: u32, arg: u32, len: u32, stdin: u32, stdout: u32, _a5: u32) -> ! {
    io::init(stdin, stdout);
    let mut counts = Counts::new();
    let ok = if len == 0 {
        if stdin == 0 {
            io::ewrite(b"usage: wc [</path>]\n");
            false
        } else {
            let mut buf = [0u8; abi::IPC_BYTES];
            loop {
                match io::read(&mut buf) {
                    Ok(0) => break true,
                    Ok(n) => counts.feed(&buf[..n]),
                    Err(_) => {
                        io::ewrite(b"wc: input stream failed\n");
                        break false;
                    }
                }
            }
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
            let chunk_bytes = 64 * 1024;
            let Ok(mut arena) = Arena::new_psram(chunk_bytes) else {
                io::ewrite(b"wc: read buffer allocation failed\n");
                io::close_stdout();
                sys::process_exit();
            };
            // SAFETY: privately owned allocation covers the complete buffer;
            // all accesses finish before arena release.
            let chunk =
                unsafe { core::slice::from_raw_parts_mut(arena.base() as *mut u8, chunk_bytes) };
            let mut offset = 0u32;
            let mut success = true;
            loop {
                match fs_read_bulk(fs, path, offset, chunk, &mut response) {
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
            let _ = arena.release();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn check(data: &[u8], expected: (u32, u32, u32)) {
        for chunk in [1, 2, 95, 96, 1024, 2048, 4096, 65536] {
            let mut counts = Counts::new();
            for block in data.chunks(chunk) {
                counts.feed(block);
            }
            assert_eq!((counts.lines, counts.words, counts.bytes), expected);
        }
    }

    #[test]
    fn counts_empty_whitespace_and_unterminated_last_line() {
        check(b"", (0, 0, 0));
        check(b" \t\n\x0b\x0c\r", (1, 0, 6));
        check(b"one two\r\nthree", (1, 3, 14));
    }

    #[test]
    fn word_and_crlf_cross_the_64_kib_boundary() {
        let mut data = vec![b'x'; 65535];
        data.extend_from_slice(b"yz \tlast\r\n");
        check(&data, (1, 2, data.len() as u32));
        let mut data = vec![b' '; 65535];
        data.extend_from_slice(b"\r\nend");
        check(&data, (1, 1, data.len() as u32));
    }

    #[test]
    fn byte_count_saturates_once_per_chunk() {
        let mut counts = Counts::new();
        counts.bytes = u32::MAX - 1;
        counts.feed(b"abc");
        assert_eq!(counts.bytes, u32::MAX);
    }
}
