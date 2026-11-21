//! Terminal input filtering shared by the shell's input sources (USB console, Tab5 Keyboard).

/// Drops ANSI escape sequences (cursor keys and the like) so they are not typed as text.
/// A lone ESC is dropped too; the byte after it is processed normally.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EscapeFilter {
    state: State,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Text,
    Escape,
    /// After `ESC [`: parameters until a final byte 0x40..=0x7e.
    Csi,
    /// After `ESC O`: exactly one more byte.
    Ss3,
}

impl EscapeFilter {
    pub const fn new() -> Self {
        Self { state: State::Text }
    }

    /// Returns `true` when `byte` is part of an escape sequence and must be ignored.
    pub fn swallow(&mut self, byte: u8) -> bool {
        match self.state {
            State::Text => {
                if byte == 0x1b {
                    self.state = State::Escape;
                    return true;
                }
                false
            }
            State::Escape => match byte {
                b'[' => {
                    self.state = State::Csi;
                    true
                }
                b'O' => {
                    self.state = State::Ss3;
                    true
                }
                0x1b => true,
                _ => {
                    self.state = State::Text;
                    false
                }
            },
            State::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.state = State::Text;
                }
                true
            }
            State::Ss3 => {
                self.state = State::Text;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &[u8]) -> ([u8; 32], usize) {
        let mut f = EscapeFilter::new();
        let mut out = [0u8; 32];
        let mut n = 0;
        for &b in input {
            if !f.swallow(b) {
                out[n] = b;
                n += 1;
            }
        }
        (out, n)
    }

    fn check(input: &[u8], want: &[u8]) {
        let (out, n) = run(input);
        assert_eq!(&out[..n], want);
    }

    #[test]
    fn plain_text_passes() {
        check(b"help\r", b"help\r");
    }

    #[test]
    fn cursor_keys_are_dropped() {
        check(b"a\x1b[Ab\x1b[1;5Dc", b"abc");
        check(b"\x1bOAx", b"x");
    }

    #[test]
    fn lone_escape_is_dropped_and_next_byte_kept() {
        check(b"\x1bx", b"x");
        check(b"\x1b\x1b[Bz", b"z");
    }

    #[test]
    fn a_sequence_may_span_two_reads() {
        let mut f = EscapeFilter::new();
        for b in [0x1b, b'[', b'1'] {
            assert!(f.swallow(b));
        }
        assert!(f.swallow(b'A'));
        assert!(!f.swallow(b'q'));
    }
}
