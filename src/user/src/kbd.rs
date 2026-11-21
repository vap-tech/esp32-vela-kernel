//! Userspace driver for the M5Stack Tab5 Keyboard (SKU A164, STM32F030C8T6) on Ext.Port1.
//!
//! The keyboard is an I2C slave at `0x6D` with a per-mode event FIFO (32 entries). In
//! Character mode the firmware resolves the Aa/Sym layers itself and queues ASCII
//! strings of up to 9 bytes preceded by a modifier byte (bit 0 Ctrl, bit 2 Alt).
//! Register map: M5Tab5-Keyboard-Internal-FW `protocol/Tab5_Keyboard-I2C-Protocol-EN-V1.0.pdf`.
//!
//! Measured on a Tab5 Keyboard, beyond what the protocol document states:
//! - register `0x40` counts one trailing byte after the string (`h` reads as length 2,
//!   `0x68 0x00`), so the text of an event is the first `length - 1` bytes;
//! - keys without a printable symbol arrive as words: `enter`, `tab`, `del`, `esc`, `up`,
//!   `down`, `left`, `right` (upper case while Aa is active);
//! - the firmware applies Ctrl itself (`Ctrl+K` arrives as `0x0b`) and still reports the
//!   modifier byte.

use crate::sys;

pub const ADDR: u8 = 0x6d;

pub const REG_INT_CFG: u8 = 0x00;
pub const REG_INT_STA: u8 = 0x01;
pub const REG_EVENT_NUM: u8 = 0x02;
pub const REG_BRIGHTNESS: u8 = 0x03;
pub const REG_MODE: u8 = 0x10;
/// RGB working mode, next to the keyboard mode register.
pub const REG_RGB_MODE: u8 = 0x11;
/// `RGB1_B, RGB1_G, RGB1_R, RGB2_B, RGB2_G, RGB2_R`, used in custom RGB mode only.
pub const REG_RGB1: u8 = 0x60;
/// The protocol table leaves one empty column between the two colour triples.
pub const REG_RGB2: u8 = 0x64;
pub const REG_CHAR_LEN: u8 = 0x40;
pub const REG_CHAR_EVENT: u8 = 0x50;
pub const REG_F0: u8 = 0xf0;

pub const MODE_CHARACTER: u8 = 2;
/// INT_CFG bit 2: interrupt only for Character-mode events.
pub const INT_CHARACTER: u8 = 0x04;

pub const RGB_BINDING: u8 = 0;
pub const RGB_CUSTOM: u8 = 1;
pub const MAX_BRIGHTNESS: u8 = 100;

pub const MOD_CTRL: u8 = 1 << 0;
pub const MOD_ALT: u8 = 1 << 2;

/// Longest string one Character-mode event can carry, including the trailing byte.
pub const MAX_CHARS: usize = 9;
/// Longest byte sequence `translate` produces (an ANSI cursor key is `ESC [ X`).
pub const MAX_OUT: usize = 3;

const EIO: i32 = -5;

/// One register transaction with the keyboard: write `tx`, then read `rx` (either may be empty).
pub trait Bus {
    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), i32>;
}

/// The keyboard reached through an `Authority::I2c(I2C_BUS_EXT1)` handle.
pub struct SysBus(pub u32);

impl Bus for SysBus {
    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), i32> {
        sys::i2c_transfer(self.0, ADDR, tx, rx)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub modifier: u8,
    len: u8,
    chars: [u8; MAX_CHARS],
}

impl Event {
    /// The key's text: the event bytes without the trailing terminator byte.
    pub fn text(&self) -> &[u8] {
        &self.chars[..usize::from(self.len).saturating_sub(1)]
    }

    /// All bytes the keyboard queued for the event, for diagnostics.
    pub fn raw(&self) -> &[u8] {
        &self.chars[..usize::from(self.len)]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
    Empty,
    Event(Event),
    /// The keyboard reported a length the protocol does not allow. Nothing was consumed;
    /// the caller retries and calls `flush` if it persists.
    BadLength(u8),
}

/// Probe the device, switch it to Character mode and drop events queued before we took over.
/// Returns the value of register `0xF0`, which the driver logs as an identification byte.
pub fn init<B: Bus>(bus: &mut B) -> Result<u8, i32> {
    let mut f0 = [0u8];
    bus.transfer(&[REG_F0], &mut f0)?;
    bus.transfer(&[REG_MODE, MODE_CHARACTER], &mut [])?;
    let mut mode = [0u8];
    bus.transfer(&[REG_MODE], &mut mode)?;
    if mode[0] & 0x03 != MODE_CHARACTER {
        return Err(EIO);
    }
    bus.transfer(&[REG_INT_CFG, INT_CHARACTER], &mut [])?;
    flush(bus)?;
    Ok(f0[0])
}

/// Clear the current mode's queue and release the interrupt line.
pub fn flush<B: Bus>(bus: &mut B) -> Result<(), i32> {
    bus.transfer(&[REG_EVENT_NUM, 0], &mut [])?;
    bus.transfer(&[REG_INT_STA, 0], &mut [])
}

/// Pop one Character-mode event. An idle keyboard costs a single register read.
pub fn poll<B: Bus>(bus: &mut B) -> Result<Poll, i32> {
    let mut len = [0u8];
    bus.transfer(&[REG_CHAR_LEN], &mut len)?;
    let n = usize::from(len[0]);
    if n == 0 {
        return Ok(Poll::Empty);
    }
    if n > MAX_CHARS {
        return Ok(Poll::BadLength(len[0]));
    }
    // modifier + n characters; reading the register pops the event.
    let mut raw = [0u8; MAX_CHARS + 1];
    bus.transfer(&[REG_CHAR_EVENT], &mut raw[..=n])?;
    let mut chars = [0u8; MAX_CHARS];
    chars[..n].copy_from_slice(&raw[1..=n]);
    Ok(Poll::Event(Event {
        modifier: raw[0],
        len: len[0],
        chars,
    }))
}

/// A colour, as the protocol document orders the channels in the register (`B, G, R`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedCommand {
    /// Firmware-driven indication (the Aa/Sym state).
    Auto,
    /// Overall RGB brightness, `0..=MAX_BRIGHTNESS`.
    Brightness(u8),
    /// Fixed colours for the two LEDs.
    Colors([Rgb; 2]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedStatus {
    pub custom: bool,
    pub brightness: u8,
    pub colors: [Rgb; 2],
}

fn parse_number(word: &[u8]) -> Option<u32> {
    if word.is_empty() || word.len() > 3 {
        return None;
    }
    word.iter().try_fold(0u32, |n, &c| {
        c.is_ascii_digit().then(|| n * 10 + u32::from(c - b'0'))
    })
}

/// Parse the argument text of the `kbdled` command:
/// `auto`, `off`, `bright N`, `R G B` (both LEDs) or `R G B R G B`.
/// Returns `None` for an empty string (a status request) and for anything malformed.
pub fn parse_led_command(args: &[u8]) -> Option<LedCommand> {
    let mut words = [&[][..]; 7];
    let mut count = 0;
    for word in args.split(|&c| c == b' ').filter(|w| !w.is_empty()) {
        *words.get_mut(count)? = word;
        count += 1;
    }
    let words = &words[..count];
    match words {
        [b"auto"] => Some(LedCommand::Auto),
        [b"off"] => Some(LedCommand::Colors([Rgb { r: 0, g: 0, b: 0 }; 2])),
        [b"bright", n] => {
            let n = u8::try_from(parse_number(n)?).ok()?;
            (n <= MAX_BRIGHTNESS).then_some(LedCommand::Brightness(n))
        }
        [_, _, _] | [_, _, _, _, _, _] => {
            let mut channel = [0u8; 6];
            for (slot, word) in channel.iter_mut().zip(words) {
                *slot = u8::try_from(parse_number(word)?).ok()?;
            }
            let first = Rgb {
                r: channel[0],
                g: channel[1],
                b: channel[2],
            };
            let second = if count == 6 {
                Rgb {
                    r: channel[3],
                    g: channel[4],
                    b: channel[5],
                }
            } else {
                first
            };
            Some(LedCommand::Colors([first, second]))
        }
        _ => None,
    }
}

/// Apply a LED command. Colours are written before custom mode is selected, so the LEDs
/// never flash a stale colour.
pub fn apply_led<B: Bus>(bus: &mut B, command: LedCommand) -> Result<(), i32> {
    match command {
        LedCommand::Auto => bus.transfer(&[REG_RGB_MODE, RGB_BINDING], &mut []),
        LedCommand::Brightness(n) => bus.transfer(&[REG_BRIGHTNESS, n], &mut []),
        LedCommand::Colors([a, b]) => {
            bus.transfer(&[REG_RGB1, a.b, a.g, a.r], &mut [])?;
            bus.transfer(&[REG_RGB2, b.b, b.g, b.r], &mut [])?;
            bus.transfer(&[REG_RGB_MODE, RGB_CUSTOM], &mut [])
        }
    }
}

pub fn read_led_status<B: Bus>(bus: &mut B) -> Result<LedStatus, i32> {
    let mut brightness = [0u8];
    let mut mode = [0u8];
    let mut first = [0u8; 3];
    let mut second = [0u8; 3];
    bus.transfer(&[REG_BRIGHTNESS], &mut brightness)?;
    bus.transfer(&[REG_RGB_MODE], &mut mode)?;
    bus.transfer(&[REG_RGB1], &mut first)?;
    bus.transfer(&[REG_RGB2], &mut second)?;
    Ok(LedStatus {
        custom: mode[0] == RGB_CUSTOM,
        brightness: brightness[0],
        colors: [
            Rgb {
                r: first[2],
                g: first[1],
                b: first[0],
            },
            Rgb {
                r: second[2],
                g: second[1],
                b: second[0],
            },
        ],
    })
}

/// Keys the firmware reports as words, and the bytes the shell expects for them.
const NAMED_KEYS: [(&[u8], &[u8]); 8] = [
    (b"enter", b"\r"),
    (b"tab", b"\t"),
    (b"del", &[0x7f]),
    (b"esc", &[0x1b]),
    (b"up", b"\x1b[A"),
    (b"down", b"\x1b[B"),
    (b"right", b"\x1b[C"),
    (b"left", b"\x1b[D"),
];

/// Convert a key event into the bytes a terminal would send. Returns the byte count, or
/// `None` when the event is not understood (the caller logs it).
pub fn translate(event: &Event, out: &mut [u8; MAX_OUT]) -> Option<usize> {
    let text = event.text();
    if let [c] = *text {
        let byte = if event.modifier & MOD_CTRL != 0 && c.is_ascii_alphabetic() {
            c & 0x1f
        } else {
            c
        };
        // Printable ASCII, or a control code the firmware produced for Ctrl+letter.
        if matches!(byte, 0x20..=0x7e | 0x01..=0x1a) {
            out[0] = byte;
            return Some(1);
        }
        return None;
    }
    for (name, bytes) in NAMED_KEYS {
        if text.eq_ignore_ascii_case(name) {
            out[..bytes.len()].copy_from_slice(bytes);
            return Some(bytes.len());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Register-level model of the keyboard: a Character-mode FIFO plus the config registers.
    struct Fake {
        present: bool,
        mode: u8,
        int_cfg: u8,
        queue: [Option<(u8, [u8; MAX_CHARS], u8)>; 8],
        flushes: u32,
        reads_of_50: u32,
        brightness: u8,
        rgb_mode: u8,
        rgb: [u8; 6],
    }

    impl Fake {
        fn new() -> Self {
            Self {
                present: true,
                mode: 0,
                int_cfg: 0x07,
                queue: [None; 8],
                flushes: 0,
                reads_of_50: 0,
                brightness: 20,
                rgb_mode: 0,
                rgb: [0; 6],
            }
        }
        /// Queue a key the way the firmware does: the string plus one trailing byte, which
        /// the length register counts.
        fn push(&mut self, modifier: u8, s: &[u8]) {
            assert!(s.len() < MAX_CHARS);
            let mut chars = [0u8; MAX_CHARS];
            chars[..s.len()].copy_from_slice(s);
            let slot = self.queue.iter_mut().find(|q| q.is_none()).unwrap();
            *slot = Some((modifier, chars, s.len() as u8 + 1));
        }
        fn head_len(&self) -> u8 {
            self.queue[0].map_or(0, |q| q.2)
        }
    }

    impl Bus for Fake {
        fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), i32> {
            if !self.present {
                return Err(-6);
            }
            match tx {
                [REG_MODE, v] => self.mode = *v,
                [REG_INT_CFG, v] => self.int_cfg = *v,
                [REG_EVENT_NUM, 0] => {
                    self.queue = [None; 8];
                    self.flushes += 1;
                }
                [REG_INT_STA, 0] => {}
                [REG_BRIGHTNESS, v] => self.brightness = *v,
                [REG_RGB_MODE, v] => self.rgb_mode = *v,
                [REG_RGB1, colors @ ..] if colors.len() == 3 => {
                    self.rgb[..3].copy_from_slice(colors);
                }
                [REG_RGB2, colors @ ..] if colors.len() == 3 => {
                    self.rgb[3..].copy_from_slice(colors);
                }
                [REG_BRIGHTNESS] => rx[0] = self.brightness,
                [REG_RGB_MODE] => rx[0] = self.rgb_mode,
                [REG_RGB1] => rx.copy_from_slice(&self.rgb[..3]),
                [REG_RGB2] => rx.copy_from_slice(&self.rgb[3..]),
                [REG_F0] => rx[0] = 0x07,
                [REG_MODE] => rx[0] = self.mode,
                [REG_CHAR_LEN] => rx[0] = self.head_len(),
                [REG_CHAR_EVENT] => {
                    self.reads_of_50 += 1;
                    let (m, chars, len) =
                        Option::take(&mut self.queue[0]).expect("read of an empty queue");
                    assert_eq!(rx.len(), usize::from(len) + 1, "read must be length + 1");
                    rx[0] = m;
                    rx[1..].copy_from_slice(&chars[..usize::from(len)]);
                    self.queue.rotate_left(1);
                }
                other => panic!("unexpected transfer {other:02x?}"),
            }
            Ok(())
        }
    }

    fn pop(kb: &mut Fake) -> Event {
        let Ok(Poll::Event(ev)) = poll(kb) else {
            panic!("expected an event");
        };
        ev
    }

    fn tr(kb: &mut Fake, modifier: u8, s: &[u8]) -> Option<([u8; MAX_OUT], usize)> {
        kb.push(modifier, s);
        let ev = pop(kb);
        let mut out = [0u8; MAX_OUT];
        translate(&ev, &mut out).map(|n| (out, n))
    }

    #[test]
    fn init_selects_character_mode_and_drops_stale_events() {
        let mut kb = Fake::new();
        kb.push(0, b"x");
        assert_eq!(init(&mut kb), Ok(0x07));
        assert_eq!(kb.mode, MODE_CHARACTER);
        assert_eq!(kb.int_cfg, INT_CHARACTER);
        assert_eq!(poll(&mut kb), Ok(Poll::Empty));
        assert_eq!(kb.reads_of_50, 0);
    }

    #[test]
    fn init_fails_when_the_keyboard_is_missing() {
        let mut kb = Fake::new();
        kb.present = false;
        assert_eq!(init(&mut kb), Err(-6));
    }

    #[test]
    fn poll_pops_events_in_order_with_modifier() {
        let mut kb = Fake::new();
        kb.push(0, b"a");
        kb.push(MOD_CTRL, b"c");
        kb.push(0, b"enter");
        let seen = [pop(&mut kb), pop(&mut kb), pop(&mut kb)];
        assert_eq!(seen[0].text(), b"a");
        assert_eq!((seen[1].modifier, seen[1].text()), (MOD_CTRL, &b"c"[..]));
        assert_eq!(seen[2].text(), b"enter");
        assert_eq!(poll(&mut kb), Ok(Poll::Empty));
    }

    #[test]
    fn the_trailing_byte_is_not_part_of_the_text() {
        let mut kb = Fake::new();
        kb.push(0, b"h");
        let ev = pop(&mut kb);
        assert_eq!(ev.raw(), &[b'h', 0]);
        assert_eq!(ev.text(), b"h");
    }

    #[test]
    fn longest_event_is_read_with_length_plus_one() {
        let mut kb = Fake::new();
        kb.push(0, b"abcdefgh");
        assert_eq!(pop(&mut kb).text(), b"abcdefgh");
    }

    #[test]
    fn oversize_length_is_reported_and_left_to_the_caller() {
        struct Bad(Fake);
        impl Bus for Bad {
            fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), i32> {
                if tx == [REG_CHAR_LEN] {
                    rx[0] = 0xff;
                    return Ok(());
                }
                self.0.transfer(tx, rx)
            }
        }
        let mut bad = Bad(Fake::new());
        assert_eq!(poll(&mut bad), Ok(Poll::BadLength(0xff)));
        assert_eq!(bad.0.flushes, 0);
        assert_eq!(flush(&mut bad), Ok(()));
        assert_eq!(bad.0.flushes, 1);
    }

    #[test]
    fn printable_characters_pass_through() {
        for c in [b'a', b'Z', b' ', b'~', b'$', b'`', b'7'] {
            assert_eq!(tr(&mut Fake::new(), 0, &[c]), Some(([c, 0, 0], 1)));
        }
    }

    #[test]
    fn named_keys_map_to_terminal_bytes() {
        let cases: [(&[u8], &[u8]); 10] = [
            (b"enter", b"\r"),
            (b"ENTER", b"\r"),
            (b"tab", b"\t"),
            (b"DEL", &[0x7f]),
            (b"del", &[0x7f]),
            (b"esc", &[0x1b]),
            (b"up", b"\x1b[A"),
            (b"down", b"\x1b[B"),
            (b"right", b"\x1b[C"),
            (b"LEFT", b"\x1b[D"),
        ];
        for (name, bytes) in cases {
            let (out, n) = tr(&mut Fake::new(), 0, name).unwrap();
            assert_eq!(&out[..n], bytes, "{name:?}");
        }
    }

    #[test]
    fn ctrl_is_applied_once() {
        // The firmware already turned Ctrl+K into 0x0b; a raw letter gets the mask here.
        assert_eq!(tr(&mut Fake::new(), 0x05, &[0x0b]), Some(([0x0b, 0, 0], 1)));
        assert_eq!(
            tr(&mut Fake::new(), MOD_CTRL, b"c"),
            Some(([0x03, 0, 0], 1))
        );
        assert_eq!(
            tr(&mut Fake::new(), MOD_CTRL, b"1"),
            Some(([b'1', 0, 0], 1))
        );
    }

    #[test]
    fn unknown_words_and_bytes_are_unmapped() {
        assert_eq!(tr(&mut Fake::new(), 0, b"f13"), None);
        assert_eq!(tr(&mut Fake::new(), 0, &[0x80]), None);
        assert_eq!(tr(&mut Fake::new(), 0, &[0x00]), None);
        assert_eq!(tr(&mut Fake::new(), 0, b""), None);
    }

    fn rgb(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }

    #[test]
    fn led_commands_parse() {
        let colors = |a, b| Some(LedCommand::Colors([a, b]));
        assert_eq!(parse_led_command(b"auto"), Some(LedCommand::Auto));
        assert_eq!(
            parse_led_command(b"off"),
            colors(rgb(0, 0, 0), rgb(0, 0, 0))
        );
        assert_eq!(
            parse_led_command(b"bright 40"),
            Some(LedCommand::Brightness(40))
        );
        assert_eq!(
            parse_led_command(b" 255   0 16 "),
            colors(rgb(255, 0, 16), rgb(255, 0, 16))
        );
        assert_eq!(
            parse_led_command(b"1 2 3 4 5 6"),
            colors(rgb(1, 2, 3), rgb(4, 5, 6))
        );
    }

    #[test]
    fn malformed_led_commands_are_rejected() {
        for bad in [
            &b""[..],
            b"bright",
            b"bright 101",
            b"bright x",
            b"256 0 0",
            b"1 2",
            b"1 2 3 4",
            b"1 2 3 4 5 6 7",
            b"-1 0 0",
            b"red",
            b"auto now",
            b"1234 0 0",
        ] {
            assert_eq!(parse_led_command(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn colors_are_written_in_bgr_order_before_custom_mode() {
        let mut kb = Fake::new();
        apply_led(&mut kb, LedCommand::Colors([rgb(1, 2, 3), rgb(4, 5, 6)])).unwrap();
        assert_eq!(kb.rgb, [3, 2, 1, 6, 5, 4]);
        assert_eq!(kb.rgb_mode, RGB_CUSTOM);
        let status = read_led_status(&mut kb).unwrap();
        assert_eq!(status.colors, [rgb(1, 2, 3), rgb(4, 5, 6)]);
        assert!(status.custom);
    }

    #[test]
    fn auto_and_brightness_touch_only_their_registers() {
        let mut kb = Fake::new();
        apply_led(&mut kb, LedCommand::Brightness(55)).unwrap();
        assert_eq!((kb.brightness, kb.rgb_mode), (55, 0));
        apply_led(&mut kb, LedCommand::Colors([rgb(9, 9, 9); 2])).unwrap();
        apply_led(&mut kb, LedCommand::Auto).unwrap();
        assert_eq!(kb.rgb_mode, RGB_BINDING);
        assert_eq!(read_led_status(&mut kb).unwrap().brightness, 55);
    }

    #[test]
    fn led_commands_report_a_missing_keyboard() {
        let mut kb = Fake::new();
        kb.present = false;
        assert_eq!(apply_led(&mut kb, LedCommand::Auto), Err(-6));
        assert_eq!(read_led_status(&mut kb), Err(-6));
    }
}
