//! Text console renderer for the Tab5 panel used in landscape with the keyboard at the bottom.
//!
//! The panel scans out a native 720x1280 RGB565 portrait raster. The landscape picture is that
//! raster rotated 90 degrees counter-clockwise (the same transform `tools/display_assets.py`
//! applies to the boot images), so landscape pixel `(u, v)` lives at native `(x = v, y = 1279 - u)`.
//! A character cell is 16x32 px: 16 native rows of 32 contiguous pixels each.
//!
//! The grid is 80x22 (the shell's line limit is 80 columns). Only cells that changed since the
//! last [`Console::render`] are drawn, so scrolling a mostly blank screen is cheap.

use crate::{font, term::EscapeFilter};

pub const FB_W: usize = 720;
pub const FB_H: usize = 1280;
pub const COLS: usize = 80;
pub const ROWS: usize = 22;
pub const CELL_W: usize = font::GLYPH_W;
pub const CELL_H: usize = font::GLYPH_H;
/// Blank rows above the grid so the 704 px of text sit centred in the 720 px height.
const TOP: usize = (FB_W - ROWS * CELL_H) / 2;

pub const FG: u16 = 0xdefb;
pub const BG: u16 = 0x0000;

const TAB: usize = 8;
const CURSOR: u16 = 0x100;

/// Words of 2 packed pixels (even pixel in the low half) covering the whole panel.
pub const FB_WORDS: usize = FB_W * FB_H / 2;

pub struct Console {
    cells: [[u8; COLS]; ROWS],
    /// What the framebuffer currently shows: character, plus [`CURSOR`] for the cursor cell.
    drawn: [[u16; COLS]; ROWS],
    col: usize,
    row: usize,
    escape: EscapeFilter,
}

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
            drawn: [[b' ' as u16; COLS]; ROWS],
            col: 0,
            row: 0,
            escape: EscapeFilter::new(),
        }
    }

    /// Cursor position as `(column, row)`; a pending wrap reports the last column.
    pub fn cursor(&self) -> (usize, usize) {
        (self.col.min(COLS - 1), self.row)
    }

    pub fn cell(&self, col: usize, row: usize) -> u8 {
        self.cells[row][col]
    }

    /// Feed terminal output: printable ASCII, `\n`, `\r`, backspace and tab. ANSI escape
    /// sequences and other control bytes are dropped.
    pub fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.escape.swallow(byte) {
                continue;
            }
            match byte {
                b'\n' => self.newline(),
                b'\r' => self.col = 0,
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

    fn put(&mut self, byte: u8) {
        if self.col == COLS {
            self.col = 0;
            self.newline();
        }
        self.cells[self.row][self.col] = byte;
        self.col += 1;
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 < ROWS {
            self.row += 1;
        } else {
            self.cells.copy_within(1.., 0);
            self.cells[ROWS - 1] = [b' '; COLS];
        }
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
                let mut want = u16::from(self.cells[row][col]);
                if row == cursor_row && col == cursor_col {
                    want |= CURSOR;
                }
                if self.drawn[row][col] != want {
                    // SAFETY: forwarded from the caller; the cell is inside the grid.
                    unsafe { draw_cell(fb, col, row, want as u8, want & CURSOR != 0) };
                    self.drawn[row][col] = want;
                    count += 1;
                }
            }
        }
        count
    }
}

/// Draw one character cell, inverted for the cursor.
///
/// # Safety
/// `fb` must point to `FB_WORDS` writable `u32` words and `col < COLS`, `row < ROWS`.
pub unsafe fn draw_cell(fb: *mut u32, col: usize, row: usize, ch: u8, inverted: bool) {
    let index = if (font::FIRST..=font::LAST).contains(&ch) {
        usize::from(ch - font::FIRST)
    } else {
        usize::from(b'?' - font::FIRST)
    };
    let glyph = &font::GLYPHS[index];
    let (fg, bg) = if inverted { (BG, FG) } else { (FG, BG) };
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
            draw_cell(a.as_mut_ptr(), 3, 3, 0x01, false);
            draw_cell(b.as_mut_ptr(), 3, 3, b'?', false);
        }
        assert_eq!(a, b);
    }
}
