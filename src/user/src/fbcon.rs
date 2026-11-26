//! Text console renderer for the Tab5 panel used in landscape with the keyboard at the bottom.
//!
//! The panel scans out a native 720x1280 RGB565 portrait raster. The landscape picture is that
//! raster rotated 90 degrees counter-clockwise (the same transform `tools/display_assets.py`
//! applies to the boot images), so landscape pixel `(u, v)` lives at native `(x = v, y = 1279 - u)`.
//! A character cell is 16x32 px: 16 native rows of 32 contiguous pixels each.
//!
//! The grid is 80x22 (the shell's line limit is 80 columns). Only cells that changed since the
//! last [`Console::render`] are drawn, so scrolling a mostly blank screen is cheap.

use crate::font;

pub const FB_W: usize = 720;
pub const FB_H: usize = 1280;
pub const COLS: usize = 80;
pub const ROWS: usize = 22;
pub const CELL_W: usize = font::GLYPH_W;
pub const CELL_H: usize = font::GLYPH_H;
/// Blank rows above the grid so the 704 px of text sit centred in the 720 px height.
const TOP: usize = (FB_W - ROWS * CELL_H) / 2;

/// Default text colour (palette index 7) and background.
pub const FG: u16 = 0xdefb;
pub const BG: u16 = 0x0000;
const DEFAULT_COLOR: u8 = 7;

/// Console inactivity (10 ms ticks) after which the backlight is switched off; the keyboard
/// service dims its LEDs after the same time without a key.
pub const IDLE_TICKS: u32 = 5 * 60 * 100;

/// The 16 ANSI colours (RGB565): 0-7 normal, 8-15 bright. Index 0 is a dark grey so black
/// text stays readable on the black background.
pub const PALETTE: [u16; 16] = [
    0x4208, 0xc986, 0x3648, 0xd5a5, 0x4b7c, 0xca99, 0x3e59, FG, // normal
    0x7bef, 0xfa8a, 0x5fed, 0xff6b, 0x7d1f, 0xfbdf, 0x6fff, 0xffff, // bright
];

const TAB: usize = 8;
const CURSOR: u16 = 0x100;

/// Words of 2 packed pixels (even pixel in the low half) covering the whole panel.
pub const FB_WORDS: usize = FB_W * FB_H / 2;

/// Escape-sequence parser state (`ESC [ params final`; `ESC O x` and others are dropped).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Esc {
    Text,
    Escape,
    Csi,
    Ss3,
}

const MAX_PARAMS: usize = 8;

pub struct Console {
    cells: [[u8; COLS]; ROWS],
    /// Palette index of each cell's text.
    colors: [[u8; COLS]; ROWS],
    /// What the framebuffer currently shows: character, [`CURSOR`] for the cursor cell and
    /// the palette index in bits 9..12.
    drawn: [[u16; COLS]; ROWS],
    col: usize,
    row: usize,
    esc: Esc,
    params: [u16; MAX_PARAMS],
    nparams: usize,
    /// Current SGR foreground (palette index 0..=7) and bold flag.
    pen: u8,
    bold: bool,
    /// Characters of a `[tag]` at the start of the line seen so far (0 = not in a tag) and
    /// their hash; the tag is coloured when its `]` arrives.
    tag_len: u8,
    tag_hash: u32,
}

/// Longest tag (with brackets) that is coloured.
const MAX_TAG: u8 = 16;
/// Palette indices a tag can get: the bright colours except red, which reads as an error.
const TAG_COLORS: [u8; 5] = [10, 11, 12, 13, 14];

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

impl Console {
    /// A blank console. The framebuffer is assumed to be zeroed (black), which is what a
    /// fresh `MEM_ALLOC` returns, so blank cells are not redrawn.
    pub const fn new() -> Self {
        Self {
            cells: [[b' '; COLS]; ROWS],
            colors: [[DEFAULT_COLOR; COLS]; ROWS],
            drawn: [[b' ' as u16 | (DEFAULT_COLOR as u16) << 9; COLS]; ROWS],
            col: 0,
            row: 0,
            esc: Esc::Text,
            params: [0; MAX_PARAMS],
            nparams: 0,
            pen: DEFAULT_COLOR,
            bold: false,
            tag_len: 0,
            tag_hash: 0,
        }
    }

    /// Tell the renderer that the raster was cleared while keeping the text grid.
    pub fn framebuffer_cleared(&mut self) {
        self.drawn
            .as_flattened_mut()
            .fill(b' ' as u16 | (DEFAULT_COLOR as u16) << 9);
    }

    /// Cursor position as `(column, row)`; a pending wrap reports the last column.
    pub fn cursor(&self) -> (usize, usize) {
        (self.col.min(COLS - 1), self.row)
    }

    pub fn cell(&self, col: usize, row: usize) -> u8 {
        self.cells[row][col]
    }

    /// Palette index of the text in a cell.
    pub fn color(&self, col: usize, row: usize) -> u8 {
        self.colors[row][col]
    }

    /// Feed terminal output: printable ASCII, `\n`, `\r`, backspace and tab, and the ANSI
    /// colour sequence `ESC [ ... m` (SGR). Other escape sequences and control bytes are dropped.
    pub fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.escape(byte) {
                continue;
            }
            match byte {
                b'\n' => self.newline(),
                b'\r' => {
                    self.col = 0;
                    self.tag_len = 0;
                }
                0x08 => self.col = self.col.min(COLS).saturating_sub(1),
                b'\t' => {
                    let stop = (self.col / TAB + 1) * TAB;
                    while self.col < stop.min(COLS) {
                        self.put(b' ');
                    }
                }
                0x20..=0x7e => self.put(byte),
                _ => {}
            }
        }
    }

    /// Returns `true` when `byte` belongs to an escape sequence.
    fn escape(&mut self, byte: u8) -> bool {
        match self.esc {
            Esc::Text => {
                if byte == 0x1b {
                    self.esc = Esc::Escape;
                    return true;
                }
                false
            }
            Esc::Escape => match byte {
                b'[' => {
                    self.esc = Esc::Csi;
                    self.params = [0; MAX_PARAMS];
                    self.nparams = 0;
                    true
                }
                b'O' => {
                    self.esc = Esc::Ss3;
                    true
                }
                0x1b => true,
                _ => {
                    self.esc = Esc::Text;
                    false
                }
            },
            Esc::Csi => {
                match byte {
                    b'0'..=b'9' => {
                        if self.nparams == 0 {
                            self.nparams = 1;
                        }
                        let p = &mut self.params[self.nparams - 1];
                        *p = p.saturating_mul(10).saturating_add(u16::from(byte - b'0'));
                    }
                    b';' => {
                        if self.nparams == 0 {
                            self.nparams = 1;
                        }
                        if self.nparams < MAX_PARAMS {
                            self.nparams += 1;
                        }
                    }
                    0x40..=0x7e => {
                        if byte == b'm' {
                            self.sgr();
                        }
                        self.esc = Esc::Text;
                    }
                    _ => {}
                }
                true
            }
            Esc::Ss3 => {
                self.esc = Esc::Text;
                true
            }
        }
    }

    /// Apply the collected SGR parameters: reset, bold and the 16 foreground colours.
    fn sgr(&mut self) {
        if self.nparams == 0 {
            self.nparams = 1; // `ESC [ m` is `ESC [ 0 m`
        }
        for i in 0..self.nparams {
            match self.params[i] {
                0 => {
                    self.pen = DEFAULT_COLOR;
                    self.bold = false;
                }
                1 => self.bold = true,
                22 => self.bold = false,
                n @ 30..=37 => self.pen = (n - 30) as u8,
                39 => self.pen = DEFAULT_COLOR,
                n @ 90..=97 => self.pen = (n - 90) as u8 + 8,
                _ => {}
            }
        }
    }

    /// Palette index for new text: bold brightens the eight normal colours.
    fn pen_color(&self) -> u8 {
        if self.bold && self.pen < 8 {
            self.pen + 8
        } else {
            self.pen
        }
    }

    fn put(&mut self, byte: u8) {
        if self.col == COLS {
            self.col = 0;
            self.newline();
        }
        self.cells[self.row][self.col] = byte;
        self.colors[self.row][self.col] = self.pen_color();
        self.track_tag(byte);
        self.col += 1;
    }

    /// Colour a `[tag]` that opens a line (kernel and service log prefixes) by its name, unless
    /// the text already has an explicit colour. Called with the cell at `col` just written.
    fn track_tag(&mut self, byte: u8) {
        if self.col == 0 && byte == b'[' && self.pen == DEFAULT_COLOR && !self.bold {
            self.tag_len = 1;
            self.tag_hash = 5381;
        } else if self.tag_len > 0 {
            if byte == b']' {
                let color = TAG_COLORS[(self.tag_hash % TAG_COLORS.len() as u32) as usize];
                self.colors[self.row][..=self.col].fill(color);
                self.tag_len = 0;
            } else if self.tag_len >= MAX_TAG || !byte.is_ascii_graphic() {
                self.tag_len = 0;
            } else {
                self.tag_hash = self.tag_hash.wrapping_mul(33).wrapping_add(u32::from(byte));
                self.tag_len += 1;
            }
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 < ROWS {
            self.row += 1;
        } else {
            self.cells.copy_within(1.., 0);
            self.cells[ROWS - 1] = [b' '; COLS];
            self.colors.copy_within(1.., 0);
            self.colors[ROWS - 1] = [DEFAULT_COLOR; COLS];
        }
        self.tag_len = 0;
    }

    /// Replace arbitrary artwork with the console in native scan order.
    /// Each destination word receives its final value once.
    ///
    /// # Safety
    /// `fb` must point to a writable full-size raster.
    pub unsafe fn replace_artwork(&mut self, fb: *mut u32) -> usize {
        let cursor = self.cursor();
        let mut count = 0;
        let mut line = [0u32; FB_W / 2];
        for y in 0..FB_H {
            line.fill(0);
            let u = FB_H - 1 - y;
            let col = u / CELL_W;
            let bit = 15 - u % CELL_W;
            for row in 0..ROWS {
                let ch = self.cells[row][col];
                let inverted = cursor == (col, row);
                let visible = inverted || ch != b' ';
                let color = if visible {
                    self.colors[row][col]
                } else {
                    DEFAULT_COLOR
                };
                if u % CELL_W == 0 {
                    self.drawn[row][col] =
                        u16::from(ch) | u16::from(color) << 9 | if inverted { CURSOR } else { 0 };
                    count += usize::from(visible);
                }
                if visible {
                    let glyph = &font::GLYPHS[usize::from(ch - font::FIRST)];
                    let rgb = PALETTE[usize::from(color)];
                    let (fg, bg) = if inverted { (BG, rgb) } else { (rgb, BG) };
                    for (dst, pair) in line[(TOP + row * CELL_H) / 2..]
                        .iter_mut()
                        .zip(glyph.chunks_exact(2))
                    {
                        *dst = u32::from(pixel(pair[0], bit, fg, bg))
                            | u32::from(pixel(pair[1], bit, fg, bg)) << 16;
                    }
                }
            }
            unsafe {
                core::ptr::copy_nonoverlapping(line.as_ptr(), fb.add(y * line.len()), line.len())
            };
        }
        count
    }

    /// Draw every cell whose picture changed. Returns how many cells were drawn.
    ///
    /// # Safety
    /// `fb` must point to `FB_WORDS` writable `u32` words (a 720x1280 RGB565 raster).
    pub unsafe fn render(&mut self, fb: *mut u32) -> usize {
        let (cursor_col, cursor_row) = self.cursor();
        let mut count = 0;
        for row in 0..ROWS {
            for col in 0..COLS {
                let ch = self.cells[row][col];
                let at_cursor = row == cursor_row && col == cursor_col;
                // A blank cell looks the same in every colour (the cursor block aside).
                let color = if ch == b' ' && !at_cursor {
                    DEFAULT_COLOR
                } else {
                    self.colors[row][col]
                };
                let mut want = u16::from(ch) | u16::from(color) << 9;
                if at_cursor {
                    want |= CURSOR;
                }
                if self.drawn[row][col] != want {
                    let rgb = PALETTE[usize::from(color)];
                    // SAFETY: forwarded from the caller; the cell is inside the grid.
                    unsafe { draw_cell(fb, col, row, want as u8, rgb, want & CURSOR != 0) };
                    self.drawn[row][col] = want;
                    count += 1;
                }
            }
        }
        count
    }
}

/// Draw one character cell in `color`, inverted for the cursor.
///
/// # Safety
/// `fb` must point to `FB_WORDS` writable `u32` words and `col < COLS`, `row < ROWS`.
pub unsafe fn draw_cell(fb: *mut u32, col: usize, row: usize, ch: u8, color: u16, inverted: bool) {
    let index = if (font::FIRST..=font::LAST).contains(&ch) {
        usize::from(ch - font::FIRST)
    } else {
        usize::from(b'?' - font::FIRST)
    };
    let glyph = &font::GLYPHS[index];
    let (fg, bg) = if inverted { (BG, color) } else { (color, BG) };
    let u0 = col * CELL_W;
    let v0 = TOP + row * CELL_H;
    for gx in 0..CELL_W {
        let y = FB_H - 1 - (u0 + gx);
        let first_word = (y * FB_W + v0) / 2;
        let bit = 15 - gx;
        for pair in 0..CELL_H / 2 {
            let lo = pixel(glyph[2 * pair], bit, fg, bg);
            let hi = pixel(glyph[2 * pair + 1], bit, fg, bg);
            // SAFETY: inside the raster by construction (u0 < 1280, v0 + 32 <= 720).
            unsafe {
                fb.add(first_word + pair)
                    .write_volatile(u32::from(lo) | (u32::from(hi) << 16));
            }
        }
    }
}

const fn pixel(row_bits: u16, bit: usize, fg: u16, bg: u16) -> u16 {
    if row_bits >> bit & 1 != 0 { fg } else { bg }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn raster() -> Vec<u32> {
        vec![0; FB_WORDS]
    }

    fn px(fb: &[u32], x: usize, y: usize) -> u16 {
        let word = fb[(y * FB_W + x) / 2];
        if x % 2 == 0 {
            word as u16
        } else {
            (word >> 16) as u16
        }
    }

    /// The pixel a landscape coordinate maps to.
    fn landscape(fb: &[u32], u: usize, v: usize) -> u16 {
        px(fb, v, FB_H - 1 - u)
    }

    fn render(console: &mut Console, fb: &mut [u32]) -> usize {
        // SAFETY: `fb` is a full-size raster.
        unsafe { console.render(fb.as_mut_ptr()) }
    }

    #[test]
    fn repaint_restores_console_after_clearing_the_export_screen() {
        let mut console = Console::new();
        let mut fb = raster();
        console.write(b"Vela");
        let count = render(&mut console, &mut fb);
        let original = fb.clone();
        fb.fill(0);
        console.framebuffer_cleared();
        assert_eq!(render(&mut console, &mut fb), count);
        assert_eq!(fb, original);
    }

    #[test]
    fn first_frame_replaces_every_artwork_pixel_and_seeds_dirty_state() {
        let mut console = Console::new();
        console.write(b"Vela\n\x1b[31mred");
        let mut expected = raster();
        render(&mut console, &mut expected);
        let mut painted = vec![0xaabbccdd; FB_WORDS];
        unsafe { console.replace_artwork(painted.as_mut_ptr()) };
        assert_eq!(expected, painted);
        assert_eq!(render(&mut console, &mut painted), 0);
    }

    #[test]
    fn grid_fits_the_panel() {
        assert_eq!(COLS * CELL_W, FB_H);
        assert!(ROWS * CELL_H <= FB_W);
        assert_eq!((FB_W - ROWS * CELL_H) % 2, 0);
        assert_eq!(TOP % 2, 0, "cells must start on a word boundary");
        assert_eq!(CELL_H % 2, 0);
    }

    #[test]
    fn glyph_pixels_land_rotated_in_the_raster() {
        let mut console = Console::new();
        let mut fb = raster();
        console.write(b"A");
        render(&mut console, &mut fb);
        let glyph = &font::GLYPHS[usize::from(b'A' - font::FIRST)];
        let mut lit = 0;
        for gy in 0..CELL_H {
            for gx in 0..CELL_W {
                // Cell (0, 0) holds the glyph; the cursor moved on to column 1.
                let want = if glyph[gy] >> (15 - gx) & 1 != 0 {
                    FG
                } else {
                    BG
                };
                assert_eq!(landscape(&fb, gx, TOP + gy), want, "gx={gx} gy={gy}");
                lit += usize::from(want == FG);
            }
        }
        assert!(lit > 20, "the glyph must have lit pixels");
    }

    #[test]
    fn cursor_is_an_inverted_cell_that_moves_with_the_text() {
        let mut console = Console::new();
        let mut fb = raster();
        render(&mut console, &mut fb);
        // Blank cell at the cursor is fully inverted: every pixel is FG.
        assert_eq!(landscape(&fb, 0, TOP), FG);
        assert_eq!(landscape(&fb, CELL_W - 1, TOP + CELL_H - 1), FG);
        console.write(b"x");
        render(&mut console, &mut fb);
        assert_eq!(console.cursor(), (1, 0));
        // The old cursor cell now shows 'x', not a solid block.
        assert_eq!(landscape(&fb, 0, TOP), BG);
        assert_eq!(landscape(&fb, CELL_W, TOP), FG);
    }

    #[test]
    fn only_changed_cells_are_redrawn() {
        let mut console = Console::new();
        let mut fb = raster();
        assert_eq!(render(&mut console, &mut fb), 1, "just the cursor");
        assert_eq!(render(&mut console, &mut fb), 0);
        console.write(b"hi");
        // 'h', 'i' and the new cursor cell.
        assert_eq!(render(&mut console, &mut fb), 3);
    }

    #[test]
    fn newline_carriage_return_and_backspace_move_the_cursor() {
        let mut console = Console::new();
        console.write(b"abc\r\nde");
        assert_eq!(console.cursor(), (2, 1));
        console.write(b"\x08 \x08");
        assert_eq!(console.cursor(), (1, 1));
        assert_eq!(console.cell(1, 1), b' ');
        console.write(b"\x08\x08\x08");
        assert_eq!(console.cursor(), (0, 1));
    }

    #[test]
    fn long_lines_wrap_and_the_screen_scrolls() {
        let mut console = Console::new();
        console.write(&[b'x'; COLS]);
        assert_eq!(console.cursor(), (COLS - 1, 0), "wrap is deferred");
        console.write(b"y");
        assert_eq!(console.cursor(), (1, 1));
        for _ in 0..ROWS {
            console.write(b"\n");
        }
        assert_eq!(console.cursor().1, ROWS - 1);
        assert_eq!(console.cell(0, 0), b' ', "first lines scrolled off");
    }

    #[test]
    fn scrolling_moves_text_up() {
        let mut console = Console::new();
        for row in 0..ROWS {
            console.write(&[b'a' + row as u8]);
            console.write(b"\r\n");
        }
        // 22 lines were written, the last newline scrolled once.
        assert_eq!(console.cell(0, 0), b'b');
        assert_eq!(console.cell(0, ROWS - 2), b'a' + (ROWS - 1) as u8);
    }

    #[test]
    fn escape_sequences_and_control_bytes_are_ignored() {
        let mut console = Console::new();
        console.write(b"a\x1b[Ab\x01\x7fc");
        assert_eq!(console.cell(0, 0), b'a');
        assert_eq!(console.cell(1, 0), b'b');
        assert_eq!(console.cell(2, 0), b'c');
        assert_eq!(console.cursor(), (3, 0));
    }

    #[test]
    fn tab_advances_to_the_next_stop() {
        let mut console = Console::new();
        console.write(b"ab\t");
        assert_eq!(console.cursor(), (8, 0));
        console.write(b"\t");
        assert_eq!(console.cursor(), (16, 0));
    }

    #[test]
    fn unprintable_cells_draw_a_question_mark() {
        let mut a = raster();
        let mut b = raster();
        // SAFETY: both are full-size rasters.
        unsafe {
            draw_cell(a.as_mut_ptr(), 3, 3, 0x01, FG, false);
            draw_cell(b.as_mut_ptr(), 3, 3, b'?', FG, false);
        }
        assert_eq!(a, b);
    }

    #[test]
    fn sgr_colours_text_and_reset_restores_the_default() {
        let mut console = Console::new();
        console.write(b"\x1b[31mA\x1b[1;32mB\x1b[0mC\x1b[93mD\x1b[mE");
        assert_eq!(console.color(0, 0), 1, "red");
        assert_eq!(console.color(1, 0), 10, "bold green is bright green");
        assert_eq!(console.color(2, 0), 7, "reset");
        assert_eq!(console.color(3, 0), 11, "bright yellow");
        assert_eq!(console.color(4, 0), 7, "ESC[m resets");
        assert_eq!(console.cursor(), (5, 0), "escape sequences take no cells");
    }

    #[test]
    fn coloured_glyphs_use_the_palette() {
        let mut console = Console::new();
        let mut fb = raster();
        console.write(b"\x1b[31mA");
        render(&mut console, &mut fb);
        let seen: Vec<u16> = (0..CELL_W)
            .flat_map(|gx| (0..CELL_H).map(move |gy| (gx, gy)))
            .map(|(gx, gy)| (gx, gy))
            .map(|(gx, gy)| landscape(&fb, gx, TOP + gy))
            .collect();
        assert!(seen.contains(&PALETTE[1]));
        assert!(!seen.contains(&FG));
    }

    #[test]
    fn recolouring_a_blank_cell_does_not_redraw_it() {
        let mut console = Console::new();
        let mut fb = raster();
        render(&mut console, &mut fb);
        console.write(b"\x1b[31m \x1b[0m");
        // The blank only moved the cursor on: old cell back to blank, new cursor cell.
        assert_eq!(render(&mut console, &mut fb), 2);
    }

    #[test]
    fn unknown_sequences_are_dropped_and_parameters_are_bounded() {
        let mut console = Console::new();
        console.write(b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12;99mx\x1b[2Jy\x1b[999999999999mz");
        assert_eq!(console.cell(0, 0), b'x');
        assert_eq!(console.cell(1, 0), b'y');
        assert_eq!(console.cell(2, 0), b'z');
    }

    #[test]
    fn log_tags_get_a_stable_colour_and_the_message_stays_default() {
        let mut console = Console::new();
        console.write(b"[kbd] hello\r\n[kbd] again\r\n[psram] x\r\n");
        let tag = console.color(0, 0);
        assert!(TAG_COLORS.contains(&tag));
        for col in 0..5 {
            assert_eq!(console.color(col, 0), tag, "all of [kbd] shares a colour");
            assert_eq!(console.color(col, 1), tag, "same tag, same colour");
        }
        assert_eq!(console.color(5, 0), 7, "the space after the tag");
        assert_eq!(console.color(6, 0), 7, "the message");
        assert!(TAG_COLORS.contains(&console.color(0, 2)));
    }

    #[test]
    fn tags_only_count_at_the_start_of_a_line_and_not_over_explicit_colours() {
        let mut console = Console::new();
        console.write(b"x [mid] \x1b[31m\r\n[red]\x1b[0m\r\n[ no end\r\n");
        assert_eq!(console.color(2, 0), 7, "not at the line start");
        assert_eq!(console.color(0, 1), 1, "an explicit colour wins");
        assert_eq!(console.color(0, 2), 7, "an unterminated tag stays default");
    }
}
