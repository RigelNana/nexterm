//! VTE sequence parser → Grid operations.
//!
//! Splits into `TermState` (implements `vte::Perform`) and `TerminalParser` (owns the VTE
//! state machine + TermState) so the borrow checker is happy.

use vte::{Params, Perform};
use tracing::trace;
use unicode_width::UnicodeWidthChar;
use crate::grid::{Grid, Cell, Color, CellAttrs, SavedCursor};

/// Terminal state that receives VTE actions and updates the grid.
pub struct TermState {
    pub grid: Grid,
    pub current_fg: Color,
    pub current_bg: Color,
    pub current_attrs: CellAttrs,
    /// Pending wrap: cursor is at the last column and the next printable char should wrap.
    pending_wrap: bool,
    /// True when the VTE state machine is (believed to be) in the ground state.
    /// Enables the fast ASCII path in `TerminalParser::process()`.
    pub(crate) in_ground: bool,
    /// Queued responses to send back to the host (e.g. DSR, DA).
    pub pending_replies: Vec<Vec<u8>>,
}

impl TermState {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            grid: Grid::new(cols, rows, 100_000),
            current_fg: Color::Default,
            current_bg: Color::Default,
            current_attrs: CellAttrs::empty(),
            pending_wrap: false,
            in_ground: true,
            pending_replies: Vec::new(),
        }
    }

    fn erase_cell(&self) -> Cell {
        Cell {
            ch: ' ',
            fg: self.current_fg,
            bg: self.current_bg,
            attrs: CellAttrs::empty(),
        }
    }

    /// Perform a linefeed: move cursor down, scrolling the region if at the bottom margin.
    fn linefeed(&mut self) {
        if self.grid.cursor_row == self.grid.scroll_bottom {
            self.grid.scroll_up_in_region();
        } else if self.grid.cursor_row < self.grid.rows - 1 {
            self.grid.cursor_row += 1;
        }
        // Clear wrap flag for the new row (do_wrap will re-set it for soft wraps).
        let r = self.grid.cursor_row;
        if r < self.grid.line_wrapped.len() {
            self.grid.line_wrapped[r] = false;
        }
        // Touch the new cursor row so that blank output lines (e.g. `echo ""`) carry a timestamp.
        self.grid.touch_active_row(r);
    }

    /// Perform a reverse index: move cursor up, scrolling the region down if at the top margin.
    fn reverse_index(&mut self) {
        if self.grid.cursor_row == self.grid.scroll_top {
            self.grid.scroll_down_in_region();
        } else if self.grid.cursor_row > 0 {
            self.grid.cursor_row -= 1;
        }
    }

    /// Advance to the next line, wrapping if needed.
    fn do_wrap(&mut self) {
        self.grid.cursor_col = 0;
        self.linefeed();
        // Mark the new row as a soft-wrap continuation (not a hard newline)
        let r = self.grid.cursor_row;
        if r < self.grid.line_wrapped.len() {
            self.grid.line_wrapped[r] = true;
        }
        self.pending_wrap = false;
    }

    /// Fast path: print a run of ASCII printable bytes (0x20..0x7E) directly
    /// to the grid, bypassing VTE state machine and Unicode width lookup.
    #[inline]
    pub(crate) fn print_ascii_run(&mut self, bytes: &[u8]) {
        let mut bi = 0;
        while bi < bytes.len() {
            if self.pending_wrap && self.grid.auto_wrap {
                self.do_wrap();
            }
            let row = self.grid.cursor_row;
            self.grid.touch_active_row(row);
            let col = self.grid.cursor_col;
            if row >= self.grid.rows || col >= self.grid.cols {
                return;
            }
            let remaining = self.grid.cols - col;
            let batch = remaining.min(bytes.len() - bi);
            let fg = self.current_fg;
            let bg = self.current_bg;
            let attrs = self.current_attrs;
            let cells = &mut self.grid.cells[row][col..col + batch];
            for (j, cell) in cells.iter_mut().enumerate() {
                *cell = Cell {
                    ch: bytes[bi + j] as char,
                    fg,
                    bg,
                    attrs,
                };
            }
            bi += batch;
            let new_col = col + batch;
            if new_col < self.grid.cols {
                self.grid.cursor_col = new_col;
            } else {
                self.grid.cursor_col = self.grid.cols - 1;
                self.pending_wrap = true;
            }
        }
    }
}

/// The outer parser that owns the VTE state machine (persisted across `process()` calls).
pub struct TerminalParser {
    vte_parser: vte::Parser,
    pub state: TermState,
}

impl TerminalParser {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            vte_parser: vte::Parser::new(),
            state: TermState::new(cols, rows),
        }
    }

    /// Feed raw bytes from PTY/SSH into the VTE state machine.
    /// Uses a fast path for runs of printable ASCII when in the ground state.
    pub fn process(&mut self, data: &[u8]) {
        self.state.grid.begin_write_batch();
        let mut i = 0;
        while i < data.len() {
            // Fast path: batch printable ASCII (0x20..0x7E) when in ground state
            if self.state.in_ground {
                let start = i;
                while i < data.len() {
                    let b = data[i];
                    if b >= 0x20 && b < 0x7f {
                        i += 1;
                    } else {
                        break;
                    }
                }
                if i > start {
                    self.state.print_ascii_run(&data[start..i]);
                    continue;
                }
                // Non-printable byte encountered — hand off to VTE
                self.state.in_ground = false;
            }
            self.vte_parser.advance(&mut self.state, data[i]);
            i += 1;
        }
    }

    /// Borrow the grid for reading.
    pub fn grid(&self) -> &Grid {
        &self.state.grid
    }

    /// Mutably borrow the grid (e.g. for resize).
    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.state.grid
    }
}

// ---------------------------------------------------------------------------
// vte::Perform implementation
// ---------------------------------------------------------------------------

impl Perform for TermState {
    fn print(&mut self, c: char) {
        self.in_ground = true;
        let char_width = UnicodeWidthChar::width(c).unwrap_or(1);

        // If pending wrap from previous char at end of line, wrap now
        if self.pending_wrap && self.grid.auto_wrap {
            self.do_wrap();
        }

        let row = self.grid.cursor_row;
        self.grid.touch_active_row(row);
        let col = self.grid.cursor_col;
        if row >= self.grid.rows || col >= self.grid.cols {
            return;
        }

        if char_width == 2 {
            // Wide character: needs 2 cells
            if col + 1 >= self.grid.cols {
                if self.grid.auto_wrap {
                    // Wrap to next line first
                    self.do_wrap();
                } else {
                    return; // Can't fit, ignore
                }
            }
            let row = self.grid.cursor_row;
            let col = self.grid.cursor_col;
            self.grid.cells[row][col] = Cell {
                ch: c,
                fg: self.current_fg,
                bg: self.current_bg,
                attrs: self.current_attrs | CellAttrs::WIDE,
            };
            if col + 1 < self.grid.cols {
                self.grid.cells[row][col + 1] = Cell {
                    ch: ' ',
                    fg: self.current_fg,
                    bg: self.current_bg,
                    attrs: self.current_attrs | CellAttrs::WIDE_TAIL,
                };
            }
            self.grid.cursor_col += 2;
            if self.grid.cursor_col >= self.grid.cols {
                self.pending_wrap = true;
                self.grid.cursor_col = self.grid.cols - 1;
            }
        } else {
            // Normal width character
            self.grid.cells[row][col] = Cell {
                ch: c,
                fg: self.current_fg,
                bg: self.current_bg,
                attrs: self.current_attrs,
            };
            if col + 1 < self.grid.cols {
                self.grid.cursor_col += 1;
            } else {
                // At last column — set pending wrap
                self.pending_wrap = true;
            }
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => {} // BEL — ignore
            0x08 => {  // BS (Backspace)
                if self.grid.cursor_col > 0 {
                    self.grid.cursor_col -= 1;
                } else if self.grid.auto_wrap
                    && self.grid.cursor_row > 0
                    && self.grid.line_wrapped.get(self.grid.cursor_row).copied().unwrap_or(false)
                {
                    // Reverse wrap: BS at col 0 of a soft-wrap continuation row
                    // moves to the end of the previous row, keeping shell/terminal
                    // cursor models in sync when deleting through wrap points.
                    self.grid.cursor_row -= 1;
                    self.grid.cursor_col = self.grid.cols.saturating_sub(1);
                }
                self.pending_wrap = false;
            }
            0x09 => {  // HT (tab)
                let next = ((self.grid.cursor_col / 8) + 1) * 8;
                self.grid.cursor_col = next.min(self.grid.cols - 1);
                self.pending_wrap = false;
            }
            0x0A | 0x0B | 0x0C => { // LF / VT / FF
                self.linefeed();
                self.pending_wrap = false;
            }
            0x0D => { // CR
                self.grid.cursor_col = 0;
                self.pending_wrap = false;
            }
            0x0E => {} // SO (Shift Out) — charset, ignore
            0x0F => {} // SI (Shift In) — charset, ignore
            _ => {
                trace!("unhandled execute byte: 0x{byte:02X}");
            }
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.in_ground = true;
        let mut p_buf = [0u16; 32];
        let mut p_len = 0usize;
        for sub in params.iter() {
            if p_len < p_buf.len() {
                p_buf[p_len] = sub[0];
                p_len += 1;
            }
        }
        let p = &p_buf[..p_len];
        let n = || p.first().copied().unwrap_or(1).max(1) as usize;
        let private = intermediates.first() == Some(&b'?');

        self.pending_wrap = false;

        match (action, private) {
            // ---- Cursor movement ----
            ('A', false) => { // CUU - Cursor Up
                self.grid.cursor_row = self.grid.cursor_row.saturating_sub(n());
            }
            ('B', false) => { // CUD - Cursor Down
                self.grid.cursor_row = (self.grid.cursor_row + n()).min(self.grid.rows - 1);
            }
            ('C', false) => { // CUF - Cursor Forward
                self.grid.cursor_col = (self.grid.cursor_col + n()).min(self.grid.cols - 1);
            }
            ('D', false) => { // CUB - Cursor Back
                self.grid.cursor_col = self.grid.cursor_col.saturating_sub(n());
            }
            ('E', false) => { // CNL - Cursor Next Line
                self.grid.cursor_row = (self.grid.cursor_row + n()).min(self.grid.rows - 1);
                self.grid.cursor_col = 0;
            }
            ('F', false) => { // CPL - Cursor Previous Line
                self.grid.cursor_row = self.grid.cursor_row.saturating_sub(n());
                self.grid.cursor_col = 0;
            }
            ('G', false) => { // CHA - Cursor Character Absolute
                let col = n().saturating_sub(1);
                self.grid.cursor_col = col.min(self.grid.cols - 1);
            }
            ('H' | 'f', false) => { // CUP - Cursor Position
                let row = p.first().copied().unwrap_or(1).max(1) as usize - 1;
                let col = p.get(1).copied().unwrap_or(1).max(1) as usize - 1;
                if self.grid.origin_mode {
                    self.grid.cursor_row = (self.grid.scroll_top + row).min(self.grid.scroll_bottom);
                } else {
                    self.grid.cursor_row = row.min(self.grid.rows - 1);
                }
                self.grid.cursor_col = col.min(self.grid.cols - 1);
            }
            ('d', false) => { // VPA - Line Position Absolute
                let row = n().saturating_sub(1);
                if self.grid.origin_mode {
                    self.grid.cursor_row = (self.grid.scroll_top + row).min(self.grid.scroll_bottom);
                } else {
                    self.grid.cursor_row = row.min(self.grid.rows - 1);
                }
            }
            ('e', false) => { // VPR - Line Position Relative
                self.grid.cursor_row = (self.grid.cursor_row + n()).min(self.grid.rows - 1);
            }
            ('`', false) => { // HPA - Character Position Absolute (same as CHA)
                let col = n().saturating_sub(1);
                self.grid.cursor_col = col.min(self.grid.cols - 1);
            }

            // ---- Erase ----
            ('J', _) => {
                let mode = p.first().copied().unwrap_or(0);
                let blank = self.erase_cell();
                match mode {
                    0 => { // Erase Below
                        self.grid.cells[self.grid.cursor_row][self.grid.cursor_col..].fill(blank);
                        for r in (self.grid.cursor_row + 1)..self.grid.rows {
                            self.grid.cells[r].fill(blank);
                            self.grid.reset_row_timestamp(r);
                        }
                    }
                    1 => { // Erase Above
                        for r in 0..self.grid.cursor_row {
                            self.grid.cells[r].fill(blank);
                            self.grid.reset_row_timestamp(r);
                        }
                        let end = self.grid.cursor_col.min(self.grid.cols - 1) + 1;
                        self.grid.cells[self.grid.cursor_row][..end].fill(blank);
                    }
                    2 | 3 => { // Erase All
                        for r in 0..self.grid.rows {
                            self.grid.cells[r].fill(blank);
                            self.grid.reset_row_timestamp(r);
                        }
                        // Reset all wrap flags on full clear (clear screen)
                        for r in 0..self.grid.line_wrapped.len() {
                            self.grid.line_wrapped[r] = false;
                        }
                        // Reset block list so stale fold markers are cleared
                        if !self.grid.is_alt_screen {
                            let abs = self.grid.scrollback.len() + self.grid.cursor_row;
                            let ts = Grid::now_timestamp();
                            self.grid.block_list.reset(abs, ts);
                        }
                    }
                    _ => {}
                }
            }
            ('K', false) => {
                let mode = p.first().copied().unwrap_or(0);
                let blank = self.erase_cell();
                let row = self.grid.cursor_row;
                match mode {
                    0 => { self.grid.cells[row][self.grid.cursor_col..].fill(blank); }
                    1 => { let end = self.grid.cursor_col.min(self.grid.cols - 1) + 1; self.grid.cells[row][..end].fill(blank); }
                    2 => { self.grid.cells[row].fill(blank); }
                    _ => {}
                }
            }

            // ---- SGR (Select Graphic Rendition) ----
            ('m', false) => {
                if p.is_empty() {
                    self.current_fg = Color::Default;
                    self.current_bg = Color::Default;
                    self.current_attrs = CellAttrs::empty();
                    return;
                }
                let mut i = 0;
                while i < p.len() {
                    match p[i] {
                        0 => {
                            self.current_fg = Color::Default;
                            self.current_bg = Color::Default;
                            self.current_attrs = CellAttrs::empty();
                        }
                        1 => self.current_attrs |= CellAttrs::BOLD,
                        2 => self.current_attrs |= CellAttrs::DIM,
                        3 => self.current_attrs |= CellAttrs::ITALIC,
                        4 => self.current_attrs |= CellAttrs::UNDERLINE,
                        5 | 6 => self.current_attrs |= CellAttrs::BLINK,
                        7 => self.current_attrs |= CellAttrs::INVERSE,
                        8 => self.current_attrs |= CellAttrs::HIDDEN,
                        9 => self.current_attrs |= CellAttrs::STRIKETHROUGH,
                        22 => self.current_attrs -= CellAttrs::BOLD | CellAttrs::DIM,
                        23 => self.current_attrs -= CellAttrs::ITALIC,
                        24 => self.current_attrs -= CellAttrs::UNDERLINE,
                        25 => self.current_attrs -= CellAttrs::BLINK,
                        27 => self.current_attrs -= CellAttrs::INVERSE,
                        28 => self.current_attrs -= CellAttrs::HIDDEN,
                        29 => self.current_attrs -= CellAttrs::STRIKETHROUGH,
                        30..=37 => self.current_fg = Color::Indexed((p[i] - 30) as u8),
                        39 => self.current_fg = Color::Default,
                        40..=47 => self.current_bg = Color::Indexed((p[i] - 40) as u8),
                        49 => self.current_bg = Color::Default,
                        90..=97 => self.current_fg = Color::Indexed((p[i] - 90 + 8) as u8),
                        100..=107 => self.current_bg = Color::Indexed((p[i] - 100 + 8) as u8),
                        38 if p.get(i + 1) == Some(&5) => {
                            if let Some(&n) = p.get(i + 2) {
                                self.current_fg = Color::Indexed(n as u8);
                                i += 2;
                            }
                        }
                        48 if p.get(i + 1) == Some(&5) => {
                            if let Some(&n) = p.get(i + 2) {
                                self.current_bg = Color::Indexed(n as u8);
                                i += 2;
                            }
                        }
                        38 if p.get(i + 1) == Some(&2) => {
                            if let (Some(&r), Some(&g), Some(&b)) =
                                (p.get(i + 2), p.get(i + 3), p.get(i + 4))
                            {
                                self.current_fg = Color::Rgb(r as u8, g as u8, b as u8);
                                i += 4;
                            }
                        }
                        48 if p.get(i + 1) == Some(&2) => {
                            if let (Some(&r), Some(&g), Some(&b)) =
                                (p.get(i + 2), p.get(i + 3), p.get(i + 4))
                            {
                                self.current_bg = Color::Rgb(r as u8, g as u8, b as u8);
                                i += 4;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }

            // ---- Insert/Delete Lines (respect scroll region) ----
            ('L', false) => { // IL - Insert Lines
                let cur = self.grid.cursor_row;
                let bot = self.grid.scroll_bottom.min(self.grid.rows - 1);
                if cur <= bot {
                    let region = bot - cur + 1;
                    let count = n().min(region);
                    self.grid.cells[cur..=bot].rotate_right(count);
                    if bot < self.grid.active_timestamps.len() {
                        self.grid.active_timestamps[cur..=bot].rotate_right(count);
                    }
                    for r in cur..cur + count {
                        self.grid.cells[r].fill(Cell::default());
                        self.grid.reset_row_timestamp(r);
                    }
                }
            }
            ('M', false) => { // DL - Delete Lines
                let cur = self.grid.cursor_row;
                let bot = self.grid.scroll_bottom.min(self.grid.rows - 1);
                if cur <= bot {
                    let region = bot - cur + 1;
                    let count = n().min(region);
                    self.grid.cells[cur..=bot].rotate_left(count);
                    if bot < self.grid.active_timestamps.len() {
                        self.grid.active_timestamps[cur..=bot].rotate_left(count);
                    }
                    for r in (bot - count + 1)..=bot {
                        self.grid.cells[r].fill(Cell::default());
                        self.grid.reset_row_timestamp(r);
                    }
                }
            }

            // ---- Scroll Up/Down ----
            ('S', false) => { // SU - Scroll Up
                self.grid.scroll_up_n_in_region(n());
            }
            ('T', false) => { // SD - Scroll Down
                self.grid.scroll_down_n_in_region(n());
            }

            // ---- Insert/Delete Characters ----
            ('@', false) => { // ICH
                let row = self.grid.cursor_row;
                let col = self.grid.cursor_col;
                let cols = self.grid.cols;
                let count = n().min(cols - col);
                // Shift cells right by count, dropping the rightmost ones
                self.grid.cells[row].copy_within(col..cols - count, col + count);
                self.grid.cells[row][col..col + count].fill(Cell::default());
            }
            ('P', false) => { // DCH
                let row = self.grid.cursor_row;
                let col = self.grid.cursor_col;
                let cols = self.grid.cols;
                let count = n().min(cols - col);
                // Shift cells left by count, filling the rightmost with blanks
                self.grid.cells[row].copy_within(col + count..cols, col);
                self.grid.cells[row][cols - count..].fill(Cell::default());
            }
            ('X', false) => { // ECH
                let count = n().min(self.grid.cols - self.grid.cursor_col);
                let row = self.grid.cursor_row;
                let blank = self.erase_cell();
                let col = self.grid.cursor_col;
                self.grid.cells[row][col..col + count].fill(blank);
            }

            // ---- DECSTBM: Set Scrolling Region ----
            ('r', false) => {
                let top = p.first().copied().unwrap_or(1).max(1) as usize - 1;
                let bot = p.get(1).copied().unwrap_or(self.grid.rows as u16).max(1) as usize - 1;
                let bot = bot.min(self.grid.rows - 1);
                if top < bot {
                    self.grid.scroll_top = top;
                    self.grid.scroll_bottom = bot;
                }
                // Move cursor to home (top-left of region if origin mode, else absolute)
                if self.grid.origin_mode {
                    self.grid.cursor_row = self.grid.scroll_top;
                } else {
                    self.grid.cursor_row = 0;
                }
                self.grid.cursor_col = 0;
            }

            // ---- Cursor Save/Restore (ANSI) ----
            ('s', false) => { // SCP - Save Cursor Position
                self.grid.saved_cursor = Some(SavedCursor {
                    row: self.grid.cursor_row,
                    col: self.grid.cursor_col,
                    fg: self.current_fg,
                    bg: self.current_bg,
                    attrs: self.current_attrs,
                });
            }
            ('u', false) => { // RCP - Restore Cursor Position
                if let Some(saved) = self.grid.saved_cursor.clone() {
                    self.grid.cursor_row = saved.row.min(self.grid.rows - 1);
                    self.grid.cursor_col = saved.col.min(self.grid.cols - 1);
                    self.current_fg = saved.fg;
                    self.current_bg = saved.bg;
                    self.current_attrs = saved.attrs;
                }
            }

            // ---- Device Status Report ----
            ('n', false) => {
                let mode = p.first().copied().unwrap_or(0);
                match mode {
                    5 => {
                        // DSR 5: operating status → "OK"
                        self.pending_replies.push(b"\x1b[0n".to_vec());
                    }
                    6 => {
                        // DSR 6: cursor position report → ESC [ row ; col R (1-based)
                        let reply = format!(
                            "\x1b[{};{}R",
                            self.grid.cursor_row + 1,
                            self.grid.cursor_col + 1,
                        );
                        self.pending_replies.push(reply.into_bytes());
                    }
                    _ => {}
                }
            }

            // ---- DA1: Send Device Attributes (CSI c / CSI 0 c) ----
            ('c', false) => {
                if p.first().copied().unwrap_or(0) == 0 {
                    // Identify as VT220 with basic feature set
                    self.pending_replies.push(b"\x1b[?62;22c".to_vec());
                }
            }

            // ---- Tab Clear ----
            ('g', false) => {
                // 0 = clear tab at cursor, 3 = clear all tabs — ignore (we use fixed 8-col tabs)
            }

            // ---- Cursor style (DECSCUSR: CSI n SP q) ----
            ('q', false) if intermediates.first() == Some(&b' ') => {
                use crate::grid::CursorStyle;
                let style = match p.first().copied().unwrap_or(0) {
                    0 | 1 => CursorStyle::Block,
                    2 => CursorStyle::Block,
                    3 | 4 => CursorStyle::Underline,
                    5 | 6 => CursorStyle::Bar,
                    _ => CursorStyle::Block,
                };
                self.grid.cursor_style = style;
            }

            // ---- Private modes: DECSET (CSI ? Ps h) ----
            ('h', true) => {
                for &mode in p {
                    match mode {
                        1 => self.grid.application_cursor_keys = true,   // DECCKM
                        6 => { // DECOM - Origin Mode
                            self.grid.origin_mode = true;
                            self.grid.cursor_row = self.grid.scroll_top;
                            self.grid.cursor_col = 0;
                        }
                        7 => self.grid.auto_wrap = true,                  // DECAWM
                        12 => {} // Cursor blink on — handled by renderer
                        25 => self.grid.cursor_visible = true,            // DECTCEM
                        47 => self.grid.enter_alt_screen(),               // Alt screen (old)
                        66 => self.grid.application_keypad = true,        // DECNKM
                        1000 => self.grid.mouse_tracking = 1000,          // Normal mouse tracking
                        1002 => self.grid.mouse_tracking = 1002,          // Button-event tracking
                        1003 => self.grid.mouse_tracking = 1003,          // Any-event tracking
                        1004 => {} // Focus events — ignore
                        1005 => {} // UTF-8 mouse — ignore
                        1006 => self.grid.mouse_format = 1006,            // SGR mouse format
                        1015 => {} // URXVT mouse — ignore
                        1047 => self.grid.enter_alt_screen(),             // Alt screen
                        1048 => { // Save cursor
                            self.grid.saved_cursor = Some(SavedCursor {
                                row: self.grid.cursor_row,
                                col: self.grid.cursor_col,
                                fg: self.current_fg,
                                bg: self.current_bg,
                                attrs: self.current_attrs,
                            });
                        }
                        1049 => { // Alt screen + save cursor
                            self.grid.saved_cursor = Some(SavedCursor {
                                row: self.grid.cursor_row,
                                col: self.grid.cursor_col,
                                fg: self.current_fg,
                                bg: self.current_bg,
                                attrs: self.current_attrs,
                            });
                            self.grid.enter_alt_screen();
                        }
                        2004 => self.grid.bracketed_paste = true,
                        _ => trace!("unhandled DECSET mode: {mode}"),
                    }
                }
            }
            // ---- Private modes: DECRST (CSI ? Ps l) ----
            ('l', true) => {
                for &mode in p {
                    match mode {
                        1 => self.grid.application_cursor_keys = false,
                        6 => {
                            self.grid.origin_mode = false;
                            self.grid.cursor_row = 0;
                            self.grid.cursor_col = 0;
                        }
                        7 => self.grid.auto_wrap = false,
                        12 => {} // Cursor blink off
                        25 => self.grid.cursor_visible = false,
                        47 => self.grid.leave_alt_screen(),
                        66 => self.grid.application_keypad = false,
                        1000 | 1002 | 1003 => self.grid.mouse_tracking = 0,
                        1004 => {}
                        1005 | 1006 | 1015 => self.grid.mouse_format = 0,
                        1047 => self.grid.leave_alt_screen(),
                        1048 => { // Restore cursor
                            if let Some(saved) = self.grid.saved_cursor.clone() {
                                self.grid.cursor_row = saved.row.min(self.grid.rows - 1);
                                self.grid.cursor_col = saved.col.min(self.grid.cols - 1);
                                self.current_fg = saved.fg;
                                self.current_bg = saved.bg;
                                self.current_attrs = saved.attrs;
                            }
                        }
                        1049 => { // Leave alt screen + restore cursor
                            self.grid.leave_alt_screen();
                            if let Some(saved) = self.grid.saved_cursor.clone() {
                                self.grid.cursor_row = saved.row.min(self.grid.rows - 1);
                                self.grid.cursor_col = saved.col.min(self.grid.cols - 1);
                                self.current_fg = saved.fg;
                                self.current_bg = saved.bg;
                                self.current_attrs = saved.attrs;
                            }
                        }
                        2004 => self.grid.bracketed_paste = false,
                        _ => trace!("unhandled DECRST mode: {mode}"),
                    }
                }
            }

            // ---- Standard modes: SM/RM (CSI Ps h / CSI Ps l) ----
            ('h', false) => {
                // Mode 4 = Insert Mode — not commonly needed
            }
            ('l', false) => {
                // Mode 4 = Replace Mode
            }

            _ => {
                trace!("unhandled CSI: params={p:?} intermediates={intermediates:?} action={action}");
            }
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.in_ground = true;
        if params.is_empty() {
            return;
        }
        // OSC 0 ; title ST — Set window title
        // OSC 2 ; title ST — Set window title
        let cmd = std::str::from_utf8(params[0]).unwrap_or("");
        match cmd {
            "0" | "2" => {
                if let Some(title_bytes) = params.get(1) {
                    if let Ok(title) = std::str::from_utf8(title_bytes) {
                        self.grid.title = title.to_string();
                    }
                }
            }
            // OSC 133 — FinalTerm semantic prompt markers (used by iTerm2, Kitty, WezTerm, ...)
            //   OSC 133 ; A ST  — prompt start   → new block (Prompt)
            //   OSC 133 ; B ST  — command start   → block → Executing
            //   OSC 133 ; C ST  — command output  → (no-op, output follows)
            //   OSC 133 ; D [;exit] ST — finished → block → Completed
            "133" => {
                if let Some(kind) = params.get(1).and_then(|b| std::str::from_utf8(b).ok()) {
                    match kind.chars().next() {
                        Some('A') => {
                            use crate::grid::BlockTrigger;
                            self.grid.start_block(BlockTrigger::Osc133A);
                        }
                        Some('B') => {
                            self.grid.mark_block_executing();
                        }
                        Some('C') => { /* output start — no action needed */ }
                        Some('D') => {
                            // Parse optional exit code.
                            // VTE splits semicolons: \e]133;D;0\a → params["133","D","0"]
                            // so the exit code is typically in params[2].
                            // Fallback: also try "D;0" format in kind itself.
                            let exit_code = params.get(2)
                                .and_then(|b| std::str::from_utf8(b).ok())
                                .and_then(|s| s.parse::<i32>().ok())
                                .or_else(|| kind.get(2..)
                                    .and_then(|s| s.trim_start_matches(';').parse::<i32>().ok()));
                            self.grid.mark_block_completed(exit_code);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.in_ground = true;
        self.pending_wrap = false;
        match (byte, intermediates.first()) {
            // RI - Reverse Index (move cursor up, scroll down if at top margin)
            (b'M', None) => {
                self.reverse_index();
            }
            // IND - Index (move cursor down, scroll up if at bottom margin)
            (b'D', None) => {
                self.linefeed();
            }
            // NEL - Next Line (CR + LF)
            (b'E', None) => {
                self.grid.cursor_col = 0;
                self.linefeed();
            }
            // DECSC - Save Cursor
            (b'7', None) => {
                self.grid.saved_cursor = Some(SavedCursor {
                    row: self.grid.cursor_row,
                    col: self.grid.cursor_col,
                    fg: self.current_fg,
                    bg: self.current_bg,
                    attrs: self.current_attrs,
                });
            }
            // DECRC - Restore Cursor
            (b'8', None) => {
                if let Some(saved) = self.grid.saved_cursor.clone() {
                    self.grid.cursor_row = saved.row.min(self.grid.rows - 1);
                    self.grid.cursor_col = saved.col.min(self.grid.cols - 1);
                    self.current_fg = saved.fg;
                    self.current_bg = saved.bg;
                    self.current_attrs = saved.attrs;
                }
            }
            // RIS - Full Reset
            (b'c', None) => {
                let cols = self.grid.cols;
                let rows = self.grid.rows;
                let max_sb = self.grid.max_scrollback;
                self.grid = Grid::new(cols, rows, max_sb);
                self.current_fg = Color::Default;
                self.current_bg = Color::Default;
                self.current_attrs = CellAttrs::empty();
            }
            // DECKPAM - Application Keypad
            (b'=', None) => {
                self.grid.application_keypad = true;
            }
            // DECKPNM - Normal Keypad
            (b'>', None) => {
                self.grid.application_keypad = false;
            }
            // Designate character set — ignore
            (b'B', Some(b'(')) | (b'0', Some(b'(')) => {}
            (b'B', Some(b')')) | (b'0', Some(b')')) => {}
            _ => {
                trace!("unhandled ESC: byte=0x{byte:02X} intermediates={intermediates:?}");
            }
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        self.in_ground = false;
    }
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {
        self.in_ground = true;
    }
}
