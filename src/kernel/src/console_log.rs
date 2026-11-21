//! Fixed-size ring of the bytes written to the system console, so a display service can
//! mirror the console without owning it. Readers keep an absolute byte cursor; bytes that
//! were overwritten before the reader got to them are skipped.

pub struct LogRing<const N: usize> {
    buf: [u8; N],
    /// Total bytes ever pushed.
    total: u32,
}

impl<const N: usize> Default for LogRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> LogRing<N> {
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            total: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.buf[self.total as usize % N] = byte;
            self.total = self.total.wrapping_add(1);
        }
    }

    /// Copy the bytes at and after `cursor` into `out`. Returns the byte count and the
    /// cursor to use next time. A cursor from the future (for example after a reader
    /// restart) is clamped to the end of the log.
    pub fn read(&self, cursor: u32, out: &mut [u8]) -> (usize, u32) {
        let oldest = self.total.saturating_sub(N as u32);
        let cursor = cursor.clamp(oldest, self.total);
        let count = out.len().min((self.total - cursor) as usize);
        for (i, slot) in out[..count].iter_mut().enumerate() {
            *slot = self.buf[(cursor as usize + i) % N];
        }
        (count, cursor + count as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_was_pushed_in_order() {
        let mut ring = LogRing::<8>::new();
        ring.push(b"abc");
        let mut out = [0u8; 8];
        assert_eq!(ring.read(0, &mut out), (3, 3));
        assert_eq!(&out[..3], b"abc");
        ring.push(b"de");
        assert_eq!(ring.read(3, &mut out), (2, 5));
        assert_eq!(&out[..2], b"de");
        assert_eq!(ring.read(5, &mut out), (0, 5));
    }

    #[test]
    fn output_buffer_limits_one_read() {
        let mut ring = LogRing::<8>::new();
        ring.push(b"abcdef");
        let mut out = [0u8; 4];
        assert_eq!(ring.read(0, &mut out), (4, 4));
        assert_eq!(&out, b"abcd");
        assert_eq!(ring.read(4, &mut out), (2, 6));
        assert_eq!(&out[..2], b"ef");
    }

    #[test]
    fn overwritten_bytes_are_skipped() {
        let mut ring = LogRing::<4>::new();
        ring.push(b"abcdef");
        let mut out = [0u8; 8];
        assert_eq!(ring.read(0, &mut out), (4, 6));
        assert_eq!(&out[..4], b"cdef");
    }

    #[test]
    fn a_cursor_from_the_future_is_clamped() {
        let mut ring = LogRing::<4>::new();
        ring.push(b"ab");
        let mut out = [0u8; 4];
        assert_eq!(ring.read(99, &mut out), (0, 2));
        ring.push(b"c");
        assert_eq!(ring.read(2, &mut out), (1, 3));
        assert_eq!(out[0], b'c');
    }
}
