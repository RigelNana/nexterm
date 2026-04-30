//! Terminal character grid: stores cells with characters, attributes, and colors.

use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// Block model (Warp-inspired)
// ---------------------------------------------------------------------------

/// Lifecycle state of a command block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    /// Prompt displayed, waiting for input.
    Prompt,
    /// Command is executing.
    Executing,
    /// Command has completed.
    Completed,
}

/// How the block boundary was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockTrigger {
    /// Implicit first block at terminal start.
    Initial,
    /// User pressed Enter (behavioral detection).
    EnterKey,
    /// OSC 133;A semantic prompt marker.
    Osc133A,
    /// First keypress detected at a new row after command output.
    InputDetected,
}

/// A single command block: prompt + command + output.
#[derive(Debug, Clone)]
pub struct Block {
    /// Unique monotonic block ID.
    pub id: u64,
    /// Absolute row where this block starts.
    pub start_row: usize,
    /// How this block was triggered.
    pub trigger: BlockTrigger,
    /// Current lifecycle state.
    pub state: BlockState,
    /// Timestamp when this block was created (HH:MM:SS).
    pub created_at: [u8; 8],
    /// Exit code from OSC 133;D, if known.
    pub exit_code: Option<i32>,
    /// Column where command input starts (cursor col at OSC 133;B).
    /// Used to strip the prompt prefix when extracting command text.
    pub cmd_col: Option<usize>,
}

/// Ordered list of command blocks.
#[derive(Debug)]
pub struct BlockList {
    blocks: Vec<Block>,
    next_id: u64,
    /// Whether OSC 133 markers have been seen. When true, Enter-key
    /// detection only transitions state (doesn't create new blocks).
    pub has_osc133: bool,
}

impl BlockList {
    pub fn new() -> Self {
        Self { blocks: Vec::new(), next_id: 0, has_osc133: false }
    }

    /// Reset the block list (e.g. on screen clear). Creates a fresh initial block.
    pub fn reset(&mut self, start_row: usize, ts: [u8; 8]) {
        self.blocks.clear();
        self.has_osc133 = false;
        let id = self.next_id;
        self.next_id += 1;
        self.blocks.push(Block {
            id,
            start_row,
            state: BlockState::Prompt,
            trigger: BlockTrigger::Initial,
            created_at: ts,
            exit_code: None,
            cmd_col: None,
        });
    }

    /// Create a new block. Finalizes the previous block first.
    /// Returns the new block's ID.
    pub fn start_block(&mut self, trigger: BlockTrigger, start_row: usize, ts: [u8; 8]) -> u64 {
        tracing::info!(
            trigger = ?trigger,
            start_row,
            has_osc133 = self.has_osc133,
            prev_state = ?self.blocks.last().map(|b| b.state),
            prev_start = self.blocks.last().map(|b| b.start_row),
            "BlockList::start_block"
        );
        // If OSC 133 active and this is just an Enter key, transition state instead.
        if self.has_osc133 && trigger == BlockTrigger::EnterKey {
            if let Some(b) = self.blocks.last_mut() {
                if b.state == BlockState::Prompt {
                    b.state = BlockState::Executing;
                }
            }
            tracing::info!("start_block: has_osc133 + EnterKey → just mark executing");
            return self.blocks.last().map(|b| b.id).unwrap_or(0);
        }
        if trigger == BlockTrigger::Osc133A { self.has_osc133 = true; }

        // Finalize previous block
        if let Some(prev) = self.blocks.last_mut() {
            if prev.state != BlockState::Completed {
                prev.state = BlockState::Completed;
            }
        }
        // Deduplicate: same row → update in place
        if let Some(last) = self.blocks.last_mut() {
            if last.start_row == start_row {
                last.trigger = trigger;
                last.state = BlockState::Prompt;
                last.created_at = ts;
                last.exit_code = None;
                last.cmd_col = None;
                return last.id;
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.blocks.push(Block {
            id, start_row, trigger,
            state: BlockState::Prompt,
            created_at: ts,
            exit_code: None,
            cmd_col: None,
        });
        id
    }

    /// Transition the current block to Executing, recording the cursor column
    /// where command input starts (i.e. right after the prompt).
    pub fn mark_executing(&mut self, cursor_col: usize) {
        if let Some(b) = self.blocks.last_mut() {
            tracing::info!(
                block_id = b.id,
                old_state = ?b.state,
                start_row = b.start_row,
                cursor_col,
                "BlockList::mark_executing (OSC 133;B)"
            );
            if b.state == BlockState::Prompt {
                b.state = BlockState::Executing;
                b.cmd_col = Some(cursor_col);
            }
        }
    }

    /// Mark current block Completed with optional exit code.
    pub fn mark_completed(&mut self, exit_code: Option<i32>) {
        if let Some(b) = self.blocks.last_mut() {
            tracing::info!(
                block_id = b.id,
                old_state = ?b.state,
                exit_code = ?exit_code,
                start_row = b.start_row,
                "BlockList::mark_completed (OSC 133;D)"
            );
            b.state = BlockState::Completed;
            b.exit_code = exit_code;
        }
    }

    /// Find the block containing `abs_row`. Returns (index, &Block).
    pub fn block_for_row(&self, abs_row: usize) -> Option<(usize, &Block)> {
        let idx = self.blocks.partition_point(|b| b.start_row <= abs_row);
        if idx == 0 { None } else { Some((idx - 1, &self.blocks[idx - 1])) }
    }

    /// Is this absolute row the start of a block?
    pub fn is_block_start(&self, abs_row: usize) -> bool {
        self.blocks.binary_search_by_key(&abs_row, |b| b.start_row).is_ok()
    }

    /// Block ID at the given row (for fold toggling).
    pub fn block_id_at_row(&self, abs_row: usize) -> Option<u64> {
        self.block_for_row(abs_row).map(|(_, b)| b.id)
    }

    /// Start row for a block ID.
    pub fn start_row_for_id(&self, id: u64) -> Option<usize> {
        self.blocks.iter().find(|b| b.id == id).map(|b| b.start_row)
    }

    /// End row (exclusive) for a block by index.
    pub fn block_end_row(&self, block_idx: usize, total_rows: usize) -> usize {
        if block_idx + 1 < self.blocks.len() {
            self.blocks[block_idx + 1].start_row
        } else {
            total_rows
        }
    }

    /// Get the current (most recent) block.
    pub fn current(&self) -> Option<&Block> { self.blocks.last() }

    /// Whether OSC 133 shell integration is active (detected at least one marker).
    pub fn has_osc133(&self) -> bool { self.has_osc133 }

    /// All blocks.
    pub fn blocks(&self) -> &[Block] { &self.blocks }

    /// Number of blocks.
    pub fn len(&self) -> usize { self.blocks.len() }

    /// Total absolute rows hidden by folds.
    /// Each folded block uses 2 visual rows (block-start + summary); the rest are hidden.
    pub fn fold_savings(
        &self,
        folds: &std::collections::HashSet<u64>,
        total_rows: usize,
        cursor_abs: usize,
    ) -> usize {
        let mut savings = 0;
        for (i, block) in self.blocks.iter().enumerate() {
            if !folds.contains(&block.id) { continue; }
            let end = self.block_end_row(i, total_rows);
            let clipped = if cursor_abs != usize::MAX && end > cursor_abs {
                cursor_abs
            } else {
                end
            };
            let rows = clipped.saturating_sub(block.start_row);
            if rows > 2 { savings += rows - 2; }
        }
        savings
    }

    /// Total virtual (fold-compressed) row count.
    /// Folded blocks contribute 2 virtual rows (block-start + summary); others are 1:1.
    pub fn virtual_total(
        &self,
        folds: &std::collections::HashSet<u64>,
        total_rows: usize,
        cursor_abs: usize,
    ) -> usize {
        let mut vtotal: usize = 0;
        for (i, block) in self.blocks.iter().enumerate() {
            let end = self.block_end_row(i, total_rows);
            let clipped = if cursor_abs != usize::MAX && end > cursor_abs { cursor_abs } else { end };
            let n = clipped.saturating_sub(block.start_row);
            vtotal += if folds.contains(&block.id) && n > 2 { 2 } else { n };
        }
        // Rows after cursor clip (live input area, not folded)
        if cursor_abs != usize::MAX && !self.blocks.is_empty() {
            let li = self.blocks.len() - 1;
            let last_end = self.block_end_row(li, total_rows);
            let clipped_end = if last_end > cursor_abs { cursor_abs } else { last_end };
            if clipped_end < total_rows {
                vtotal += total_rows - clipped_end;
            }
        }
        if self.blocks.is_empty() { vtotal = total_rows; }
        vtotal
    }

    /// Compute the absolute row to start rendering from, treating `scroll_offset`
    /// as a virtual (fold-compressed) offset from the bottom.
    pub fn render_start(
        &self,
        folds: &std::collections::HashSet<u64>,
        total_rows: usize,
        cursor_abs: usize,
        visible_rows: usize,
        scroll_offset: usize,
    ) -> usize {
        let vtotal = self.virtual_total(folds, total_rows, cursor_abs);
        let target_vrow = vtotal.saturating_sub(visible_rows + scroll_offset);

        // Walk forward through blocks to find the absolute row at target_vrow
        let mut vrow: usize = 0;
        for (i, block) in self.blocks.iter().enumerate() {
            let end = self.block_end_row(i, total_rows);
            let clipped = if cursor_abs != usize::MAX && end > cursor_abs { cursor_abs } else { end };
            let abs_n = clipped.saturating_sub(block.start_row);
            let vn = if folds.contains(&block.id) && abs_n > 2 { 2 } else { abs_n };

            if target_vrow < vrow + vn {
                let offset = target_vrow - vrow;
                if folds.contains(&block.id) && abs_n > 2 {
                    // Inside folded block: vrow 0 = block-start, vrow 1 = summary
                    return if offset == 0 { block.start_row } else { block.start_row + 1 };
                } else {
                    return block.start_row + offset;
                }
            }
            vrow += vn;
        }
        // Past all blocks — remaining rows are 1:1
        if !self.blocks.is_empty() {
            let li = self.blocks.len() - 1;
            let last_end = self.block_end_row(li, total_rows);
            let clipped_end = if cursor_abs != usize::MAX && last_end > cursor_abs { cursor_abs } else { last_end };
            return clipped_end + (target_vrow - vrow);
        }
        total_rows.saturating_sub(visible_rows + scroll_offset)
    }

    /// Adjust start_rows when `n` lines are evicted from scrollback front.
    pub fn adjust_on_evict(&mut self, n: usize) {
        self.blocks.retain_mut(|b| {
            if b.start_row < n { false } else { b.start_row -= n; true }
        });
    }
}

/// A single cell in the terminal grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: CellAttrs,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attrs: CellAttrs::empty(),
        }
    }
}

/// Terminal color representation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

bitflags::bitflags! {
    /// Cell text attributes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CellAttrs: u16 {
        const BOLD       = 0b0000_0001;
        const DIM        = 0b0000_0010;
        const ITALIC     = 0b0000_0100;
        const UNDERLINE  = 0b0000_1000;
        const BLINK      = 0b0001_0000;
        const INVERSE    = 0b0010_0000;
        const HIDDEN     = 0b0100_0000;
        const STRIKETHROUGH = 0b1000_0000;
        const WIDE       = 0b0001_0000_0000;
        const WIDE_TAIL  = 0b0010_0000_0000;
    }
}

/// Cursor visual style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorStyle {
    Block,
    Underline,
    Bar,
}

impl Default for CursorStyle {
    fn default() -> Self {
        Self::Block
    }
}

/// A rectangular text selection (inclusive endpoints).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Anchor point (where mouse-down occurred).
    pub start_row: usize,
    pub start_col: usize,
    /// Current extent (where mouse currently is).
    pub end_row: usize,
    pub end_col: usize,
}

impl Selection {
    /// Return (top_row, top_col, bottom_row, bottom_col) in normalized order.
    pub fn ordered(&self) -> (usize, usize, usize, usize) {
        if (self.start_row, self.start_col) <= (self.end_row, self.end_col) {
            (self.start_row, self.start_col, self.end_row, self.end_col)
        } else {
            (self.end_row, self.end_col, self.start_row, self.start_col)
        }
    }

    /// Check if a given cell is within this selection.
    pub fn contains(&self, row: usize, col: usize) -> bool {
        let (r0, c0, r1, c1) = self.ordered();
        if row < r0 || row > r1 {
            return false;
        }
        if row == r0 && row == r1 {
            return col >= c0 && col <= c1;
        }
        if row == r0 {
            return col >= c0;
        }
        if row == r1 {
            return col <= c1;
        }
        true // middle rows fully selected
    }
}

/// Saved state when switching to alternate screen buffer.
#[derive(Debug)]
struct AltScreenSaved {
    cells: Vec<Vec<Cell>>,
    scrollback: VecDeque<Vec<Cell>>,
    scrollback_timestamps: VecDeque<[u8; 8]>,
    scrollback_wrapped: VecDeque<bool>,
    active_timestamps: Vec<[u8; 8]>,
    line_wrapped: Vec<bool>,
    cursor_row: usize,
    cursor_col: usize,
    scroll_top: usize,
    scroll_bottom: usize,
}

/// Saved cursor state (DECSC/DECRC).
#[derive(Debug, Clone)]
pub struct SavedCursor {
    pub row: usize,
    pub col: usize,
    pub fg: Color,
    pub bg: Color,
    pub attrs: CellAttrs,
}

/// The terminal grid: `rows × cols` matrix of cells, plus a scrollback buffer.
#[derive(Debug)]
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    /// Active screen buffer (visible).
    pub cells: Vec<Vec<Cell>>,
    /// Scrollback lines (newest at back). VecDeque for O(1) trim from front.
    pub scrollback: VecDeque<Vec<Cell>>,
    /// Timestamp for each scrollback line (parallel to `scrollback`).
    pub scrollback_timestamps: VecDeque<[u8; 8]>,
    pub max_scrollback: usize,
    /// Cursor position.
    pub cursor_row: usize,
    pub cursor_col: usize,
    /// Cursor style (block, underline, bar).
    pub cursor_style: CursorStyle,
    /// Whether the cursor is visible (DECTCEM).
    pub cursor_visible: bool,
    /// Active text selection, if any.
    pub selection: Option<Selection>,
    /// Viewport scroll offset: 0 = bottom (live), >0 = scrolled into history.
    pub scroll_offset: usize,

    // ---- Alternate screen buffer ----
    /// Saved primary screen buffer (cells, scrollback, cursor) when alt screen is active.
    alt_screen_saved: Option<AltScreenSaved>,
    pub is_alt_screen: bool,

    // ---- Scroll region (DECSTBM) ----
    /// Top margin (0-indexed, inclusive).
    pub scroll_top: usize,
    /// Bottom margin (0-indexed, inclusive).
    pub scroll_bottom: usize,

    // ---- Saved cursor (DECSC/DECRC) ----
    pub saved_cursor: Option<SavedCursor>,

    // ---- Modes ----
    /// Auto-wrap mode (DECAWM, mode 7). When true, printing past last col wraps.
    pub auto_wrap: bool,
    /// Origin mode (DECOM, mode 6). When true, cursor positions are relative to scroll region.
    pub origin_mode: bool,
    /// Bracketed paste mode (mode 2004).
    pub bracketed_paste: bool,
    /// Application cursor keys (DECCKM, mode 1).
    pub application_cursor_keys: bool,
    /// Application keypad mode (DECNKM).
    pub application_keypad: bool,
    /// Mouse tracking modes
    pub mouse_tracking: u16,   // 0=off, 1000=normal, 1002=button, 1003=any
    pub mouse_format: u16,     // 0=default, 1006=SGR
    /// Window title.
    pub title: String,
    /// Timestamp for each active screen row (parallel to `cells`).
    pub active_timestamps: Vec<[u8; 8]>,
    /// Cached timestamp for the current write batch (set once per process() call).
    batch_timestamp: [u8; 8],
    /// Per-row soft-wrap flag: true if row is a continuation of the previous (no hard newline).
    pub line_wrapped: Vec<bool>,
    /// Soft-wrap flag for scrollback rows (parallel to `scrollback`).
    pub scrollback_wrapped: VecDeque<bool>,
    /// Block list: ordered command blocks (Warp-inspired model).
    pub block_list: BlockList,
}

impl Grid {
    pub fn new(cols: usize, rows: usize, max_scrollback: usize) -> Self {
        let cells = vec![vec![Cell::default(); cols]; rows];
        Self {
            cols,
            rows,
            cells,
            scrollback: VecDeque::new(),
            scrollback_timestamps: VecDeque::new(),
            max_scrollback,
            cursor_row: 0,
            cursor_col: 0,
            cursor_style: CursorStyle::default(),
            cursor_visible: true,
            selection: None,
            scroll_offset: 0,
            alt_screen_saved: None,
            is_alt_screen: false,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            saved_cursor: None,
            auto_wrap: true,
            origin_mode: false,
            bracketed_paste: false,
            application_cursor_keys: false,
            application_keypad: false,
            mouse_tracking: 0,
            mouse_format: 0,
            title: String::new(),
            active_timestamps: vec![[b'0'; 8]; rows],
            batch_timestamp: [b'0'; 8],
            line_wrapped: vec![false; rows],
            scrollback_wrapped: VecDeque::new(),
            block_list: {
                let mut bl = BlockList::new();
                bl.start_block(BlockTrigger::Initial, 0, [b'0'; 8]);
                bl
            },
        }
    }

    /// Resize the grid, reflowing content as needed.
    pub fn resize(&mut self, new_cols: usize, new_rows: usize) {
        self.cols = new_cols;
        self.rows = new_rows;
        self.cells.resize(new_rows, vec![Cell::default(); new_cols]);
        for row in &mut self.cells {
            row.resize(new_cols, Cell::default());
        }
        self.active_timestamps.resize(new_rows, [b'0'; 8]);
        self.line_wrapped.resize(new_rows, false);
        self.scroll_bottom = new_rows.saturating_sub(1);
        if self.cursor_row >= new_rows {
            self.cursor_row = new_rows.saturating_sub(1);
        }
        if self.cursor_col >= new_cols {
            self.cursor_col = new_cols.saturating_sub(1);
        }
    }

    /// Scroll the visible buffer up by one line, respecting scroll region.
    /// If the scroll region is the full screen, push into scrollback.
    pub fn scroll_up(&mut self) {
        self.scroll_up_in_region();
    }

    /// Scroll up within the current scroll region.
    pub fn scroll_up_in_region(&mut self) {
        self.scroll_up_n_in_region(1);
    }

    /// Scroll up within the current scroll region by `n` lines.
    pub fn scroll_up_n_in_region(&mut self, n: usize) {
        let top = self.scroll_top;
        let bot = self.scroll_bottom.min(self.rows.saturating_sub(1));
        if top >= self.cells.len() || bot >= self.cells.len() || top > bot {
            return;
        }
        let region = bot - top + 1;
        let n = n.min(region);
        if n == 0 { return; }

        self.cells[top..=bot].rotate_left(n);
        // Keep timestamps and wrap flags in lock-step with cells.
        if bot < self.active_timestamps.len() {
            self.active_timestamps[top..=bot].rotate_left(n);
        }
        if bot < self.line_wrapped.len() {
            self.line_wrapped[top..=bot].rotate_left(n);
        }

        if top == 0 && bot == self.rows.saturating_sub(1) && !self.is_alt_screen {
            // Full-screen scroll: push the n scrolled-off rows to scrollback.
            let cols = self.cols;
            let now_ts = Self::now_timestamp();
            for i in (bot - n + 1)..=bot {
                let new_blank = if self.scrollback.len() >= self.max_scrollback {
                    self.scrollback_timestamps.pop_front();
                    self.scrollback_wrapped.pop_front();
                    self.block_list.adjust_on_evict(1);
                    self.scrollback.pop_front().map(|mut row| {
                        row.clear();
                        row.resize(cols, Cell::default());
                        row
                    })
                } else {
                    None
                };
                let blank = new_blank.unwrap_or_else(|| vec![Cell::default(); cols]);
                let top_row = std::mem::replace(&mut self.cells[i], blank);
                self.scrollback.push_back(top_row);
                // Use the active row's timestamp if available, otherwise now
                let row_ts = if i < self.active_timestamps.len() && self.active_timestamps[i] != [b'0'; 8] {
                    self.active_timestamps[i]
                } else {
                    now_ts
                };
                self.scrollback_timestamps.push_back(row_ts);
                // Carry wrap flag to scrollback, then reset
                let was_wrapped = if i < self.line_wrapped.len() { self.line_wrapped[i] } else { false };
                self.scrollback_wrapped.push_back(was_wrapped);
                // Reset the active timestamp and wrap flag for the now-blank row
                if i < self.active_timestamps.len() {
                    self.active_timestamps[i] = [b'0'; 8];
                }
                if i < self.line_wrapped.len() {
                    self.line_wrapped[i] = false;
                }
            }
        } else {
            // Region scroll: clear the last n rows in place.
            for r in (bot - n + 1)..=bot {
                self.cells[r].fill(Cell::default());
                self.reset_row_timestamp(r);
            }
        }
    }

    /// Scroll down within the current scroll region by `n` lines.
    pub fn scroll_down_n_in_region(&mut self, n: usize) {
        let top = self.scroll_top;
        let bot = self.scroll_bottom.min(self.rows.saturating_sub(1));
        if top >= self.cells.len() || bot >= self.cells.len() || top > bot {
            return;
        }
        let region = bot - top + 1;
        let n = n.min(region);
        if n == 0 { return; }

        self.cells[top..=bot].rotate_right(n);
        if bot < self.active_timestamps.len() {
            self.active_timestamps[top..=bot].rotate_right(n);
        }
        if bot < self.line_wrapped.len() {
            self.line_wrapped[top..=bot].rotate_right(n);
        }
        // Clear the first n rows in place.
        for r in top..top + n {
            self.cells[r].fill(Cell::default());
            self.reset_row_timestamp(r);
            if r < self.line_wrapped.len() { self.line_wrapped[r] = false; }
        }
    }

    /// Scroll down within the current scroll region.
    pub fn scroll_down_in_region(&mut self) {
        self.scroll_down_n_in_region(1);
    }

    /// Switch to alternate screen buffer.
    pub fn enter_alt_screen(&mut self) {
        if self.is_alt_screen {
            return;
        }
        let saved = AltScreenSaved {
            cells: std::mem::replace(
                &mut self.cells,
                vec![vec![Cell::default(); self.cols]; self.rows],
            ),
            scrollback: std::mem::take(&mut self.scrollback),
            scrollback_timestamps: std::mem::take(&mut self.scrollback_timestamps),
            scrollback_wrapped: std::mem::take(&mut self.scrollback_wrapped),
            active_timestamps: std::mem::replace(&mut self.active_timestamps, vec![[b'0'; 8]; self.rows]),
            line_wrapped: std::mem::replace(&mut self.line_wrapped, vec![false; self.rows]),
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
        };
        self.alt_screen_saved = Some(saved);
        self.is_alt_screen = true;
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
    }

    /// Switch back to primary screen buffer.
    pub fn leave_alt_screen(&mut self) {
        if !self.is_alt_screen {
            return;
        }
        if let Some(saved) = self.alt_screen_saved.take() {
            self.cells = saved.cells;
            self.scrollback = saved.scrollback;
            self.scrollback_timestamps = saved.scrollback_timestamps;
            self.scrollback_wrapped = saved.scrollback_wrapped;
            self.active_timestamps = saved.active_timestamps;
            self.line_wrapped = saved.line_wrapped;
            self.cursor_row = saved.cursor_row.min(self.rows.saturating_sub(1));
            self.cursor_col = saved.cursor_col.min(self.cols.saturating_sub(1));
            self.scroll_top = saved.scroll_top;
            self.scroll_bottom = saved.scroll_bottom;
        }
        self.is_alt_screen = false;
    }

    /// Scroll viewport up (into history) by `lines` lines.
    pub fn scroll_viewport_up(&mut self, lines: usize) {
        let max = self.scrollback.len();
        self.scroll_offset = (self.scroll_offset + lines).min(max);
    }

    /// Scroll viewport down (toward live) by `lines` lines.
    pub fn scroll_viewport_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
    }

    /// Reset viewport to live (bottom).
    pub fn scroll_reset(&mut self) {
        self.scroll_offset = 0;
    }

    /// Convert a viewport row (0 = top of screen) to an absolute row index.
    /// Absolute row 0 = first scrollback line; scrollback.len() = first screen line.
    pub fn viewport_to_absolute(&self, visible_row: usize) -> usize {
        let sb_len = self.scrollback.len();
        sb_len.saturating_sub(self.scroll_offset) + visible_row
    }

    /// Absolute index of the first visible row in the viewport.
    pub fn viewport_start(&self) -> usize {
        self.scrollback.len().saturating_sub(self.scroll_offset)
    }

    /// Get row data by absolute index. Returns None if out of range.
    pub fn absolute_row(&self, abs_row: usize) -> Option<&Vec<Cell>> {
        let sb_len = self.scrollback.len();
        if abs_row < sb_len {
            self.scrollback.get(abs_row)
        } else {
            self.cells.get(abs_row - sb_len)
        }
    }

    /// Get a row for rendering, accounting for scroll offset.
    /// Row 0 = top of viewport. Returns None if out of range.
    pub fn viewport_row(&self, visible_row: usize) -> Option<&Vec<Cell>> {
        if self.scroll_offset == 0 {
            self.cells.get(visible_row)
        } else {
            // Total logical lines = scrollback + screen
            let sb_len = self.scrollback.len();
            // The viewport starts at (sb_len - scroll_offset)
            let logical = sb_len.saturating_sub(self.scroll_offset) + visible_row;
            if logical < sb_len {
                self.scrollback.get(logical)
            } else {
                self.cells.get(logical - sb_len)
            }
        }
    }

    /// Generate a HH:MM:SS timestamp for the current moment.
    pub fn now_timestamp() -> [u8; 8] {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // UTC time — good enough for log display
        let s = (secs % 60) as u8;
        let m = ((secs / 60) % 60) as u8;
        let h = ((secs / 3600) % 24) as u8;
        let mut buf = [b'0'; 8];
        buf[0] = b'0' + h / 10;
        buf[1] = b'0' + h % 10;
        buf[2] = b':';
        buf[3] = b'0' + m / 10;
        buf[4] = b'0' + m % 10;
        buf[5] = b':';
        buf[6] = b'0' + s / 10;
        buf[7] = b'0' + s % 10;
        buf
    }

    /// Get the timestamp for a given absolute row.
    /// Returns the stored timestamp for scrollback rows and active screen rows.
    pub fn row_timestamp(&self, abs_row: usize) -> Option<&[u8; 8]> {
        let sb_len = self.scrollback.len();
        if abs_row < sb_len {
            self.scrollback_timestamps.get(abs_row)
        } else {
            let screen_row = abs_row - sb_len;
            self.active_timestamps.get(screen_row).filter(|ts| **ts != [b'0'; 8])
        }
    }

    /// Called once at the start of each data batch (process() call) to snapshot
    /// the current time.  All rows written during this batch share this timestamp.
    #[inline]
    pub fn begin_write_batch(&mut self) {
        self.batch_timestamp = Self::now_timestamp();
    }

    /// Mark an active screen row as having received output.
    /// Uses first-write semantics: only sets the timestamp if the row has none yet.
    #[inline]
    pub fn touch_active_row(&mut self, row: usize) {
        if row < self.active_timestamps.len() && self.active_timestamps[row] == [b'0'; 8] {
            self.active_timestamps[row] = self.batch_timestamp;
        }
    }

    /// Reset the timestamp for an active screen row (called on erase/clear).
    #[inline]
    pub fn reset_row_timestamp(&mut self, row: usize) {
        if row < self.active_timestamps.len() {
            self.active_timestamps[row] = [b'0'; 8];
        }
    }

    // ---- Block lifecycle API ----

    /// Start a new block at the current cursor row with the given trigger.
    /// Finalises the previous block. Returns the new block ID.
    pub fn start_block(&mut self, trigger: BlockTrigger) -> u64 {
        let abs_row = self.scrollback.len() + self.cursor_row;
        let ts = Self::now_timestamp();
        self.block_list.start_block(trigger, abs_row, ts)
    }

    /// Transition current block to Executing (called on OSC 133;B).
    /// Records the cursor column so we know where the prompt ends.
    pub fn mark_block_executing(&mut self) {
        let col = self.cursor_col;
        self.block_list.mark_executing(col);
    }

    /// Mark current block Completed with optional exit code (OSC 133;D).
    pub fn mark_block_completed(&mut self, exit_code: Option<i32>) {
        self.block_list.mark_completed(exit_code);
    }

    /// Delegate: is this absolute row the start of a block?
    pub fn is_block_start(&self, abs_row: usize) -> bool {
        self.block_list.is_block_start(abs_row)
    }

    /// Raw wrap flag (not adjusted for blank content).
    fn raw_wrap_flag(&self, abs_row: usize) -> bool {
        let sb = self.scrollback.len();
        if abs_row < sb {
            self.scrollback_wrapped.get(abs_row).copied().unwrap_or(false)
        } else {
            let screen_row = abs_row - sb;
            self.line_wrapped.get(screen_row).copied().unwrap_or(false)
        }
    }

    /// Check if the previous row's last column is non-blank (i.e. it overflowed).
    fn prev_row_fills_last_col(&self, abs_row: usize) -> bool {
        if abs_row == 0 { return false; }
        self.absolute_row(abs_row - 1)
            .and_then(|prev| prev.last())
            .map(|c| c.ch != ' ' && c.ch != '\0')
            .unwrap_or(false)
    }

    /// Check if an absolute row is a soft-wrap continuation (not a new logical line).
    /// Requires both: the wrap flag is set AND the previous row's last column is filled.
    /// This auto-corrects when content is deleted and the previous row no longer overflows.
    pub fn is_row_wrapped(&self, abs_row: usize) -> bool {
        self.raw_wrap_flag(abs_row)
            && !self.is_row_blank(abs_row)
            && self.prev_row_fills_last_col(abs_row)
    }

    /// True if this row is a phantom wrap continuation: had its wrap flag set,
    /// but is now blank OR the previous row no longer overflows. Such rows
    /// should be hidden in log mode to clean up after deletions.
    pub fn is_phantom_wrap(&self, abs_row: usize) -> bool {
        self.raw_wrap_flag(abs_row)
            && (self.is_row_blank(abs_row) || !self.prev_row_fills_last_col(abs_row))
    }

    /// Check if an absolute row is entirely blank.
    pub fn is_row_blank(&self, abs_row: usize) -> bool {
        self.absolute_row(abs_row)
            .map(|row| row.iter().all(|c| c.ch == ' ' || c.ch == '\0'))
            .unwrap_or(true)
    }

    /// Extract the text content of a row as a String.
    fn row_text(row: &[Cell]) -> String {
        row.iter().map(|c| c.ch).collect()
    }

    /// Extract the trimmed text of an absolute row (public).
    pub fn row_text_at(&self, abs_row: usize) -> Option<String> {
        self.absolute_row(abs_row)
            .map(|row| Self::row_text(row).trim_end().to_string())
    }

    /// Extract the command text for a completed block.
    /// Collects text from the block's start row up to the next block (or to the
    /// first blank row), trimming trailing whitespace.  Returns None if the
    /// block index is out of range or the text is empty.
    pub fn block_command_text(&self, block_idx: usize) -> Option<String> {
        let blocks = self.block_list.blocks();
        let block = blocks.get(block_idx)?;
        let end = self.block_list.block_end_row(block_idx, self.total_rows());
        // Use cmd_col (set by OSC 133;B) to skip the prompt prefix.
        let cmd_col = block.cmd_col.unwrap_or(0);
        let max_rows = (end - block.start_row).min(4);
        let mut lines = Vec::new();
        for (i, r) in (block.start_row..block.start_row + max_rows).enumerate() {
            if let Some(text) = self.row_text_at(r) {
                if text.is_empty() { break; }
                // First row: skip prompt prefix up to cmd_col
                let line = if i == 0 && cmd_col > 0 {
                    let char_idx = text.char_indices()
                        .nth(cmd_col)
                        .map(|(idx, _)| idx)
                        .unwrap_or(text.len());
                    text[char_idx..].to_string()
                } else {
                    text
                };
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    lines.push(trimmed.to_string());
                }
            }
        }
        let full = lines.join(" ");
        if full.is_empty() { None } else { Some(full) }
    }

    /// Total number of logical rows (scrollback + active screen).
    pub fn total_rows(&self) -> usize {
        self.scrollback.len() + self.cells.len()
    }

    /// Map a visual row (0 = top of viewport) to the corresponding absolute
    /// row, accounting for fold compression. Walks forward from the adjusted
    /// viewport start, skipping folded ranges exactly as the renderer does.
    pub fn visual_to_absolute(
        &self,
        target_visual: usize,
        folds: &std::collections::HashSet<u64>,
    ) -> usize {
        let total = self.total_rows();
        let cursor_abs = self.scrollback.len() + self.cursor_row;
        let mut abs = self.block_list.render_start(folds, total, cursor_abs, self.rows, self.scroll_offset);
        let mut vis: usize = 0;

        while vis < target_visual && abs < total {
            // Skip phantom wrap rows (matches renderer behavior); never skip cursor row.
            if abs != cursor_abs && self.is_phantom_wrap(abs) {
                abs += 1;
                continue;
            }
            let is_start = self.block_list.is_block_start(abs);
            if !is_start {
                if let Some((bi, blk)) = self.block_list.block_for_row(abs) {
                    if folds.contains(&blk.id) {
                        let mut end = self.block_list.block_end_row(bi, total);
                        if end > cursor_abs { end = cursor_abs; }
                        let count = end.saturating_sub(abs);
                        if count > 0 {
                            vis += 1; // summary line occupies 1 visual row
                            abs = end;
                            continue;
                        }
                    }
                }
            }
            vis += 1;
            abs += 1;
        }
        abs
    }

    /// Search all rows (scrollback + screen) for matches.
    /// Returns `(abs_row, start_col, match_len)` for every match found.
    ///
    /// `case_sensitive` — respect letter case.
    /// `whole_word` — match only at word boundaries.
    /// `use_regex` — interpret `query` as a regex pattern.
    pub fn search_text(
        &self,
        query: &str,
        case_sensitive: bool,
        whole_word: bool,
        use_regex: bool,
    ) -> Vec<(usize, usize, usize)> {
        if query.is_empty() {
            return Vec::new();
        }

        let mut results = Vec::new();
        let total = self.total_rows();

        // Pre-compile regex or prepare needle once
        let regex_re = if use_regex {
            let pattern = if whole_word {
                format!(r"\b(?:{})\b", query)
            } else {
                query.to_string()
            };
            regex::RegexBuilder::new(&pattern)
                .case_insensitive(!case_sensitive)
                .build()
                .ok()
        } else {
            None
        };

        let needle_lower = if !case_sensitive && !use_regex {
            query.to_lowercase()
        } else {
            String::new()
        };

        for abs_row in 0..total {
            let row = if abs_row < self.scrollback.len() {
                &self.scrollback[abs_row]
            } else {
                &self.cells[abs_row - self.scrollback.len()]
            };
            let text = Self::row_text(row);

            if use_regex {
                if let Some(re) = &regex_re {
                    for m in re.find_iter(&text) {
                        results.push((abs_row, m.start(), m.len()));
                    }
                }
            } else {
                // Plain text search
                let haystack = if case_sensitive {
                    text.clone()
                } else {
                    text.to_lowercase()
                };
                let needle = if case_sensitive { query } else { &needle_lower };

                let mut start = 0;
                while let Some(pos) = haystack[start..].find(needle) {
                    let col = start + pos;
                    let mlen = needle.len();

                    if whole_word {
                        let before_ok = col == 0
                            || !haystack.as_bytes()[col - 1].is_ascii_alphanumeric();
                        let after_ok = col + mlen >= haystack.len()
                            || !haystack.as_bytes()[col + mlen].is_ascii_alphanumeric();
                        if before_ok && after_ok {
                            results.push((abs_row, col, mlen));
                        }
                    } else {
                        results.push((abs_row, col, mlen));
                    }
                    start = col + 1;
                }
            }
        }
        results
    }
}
