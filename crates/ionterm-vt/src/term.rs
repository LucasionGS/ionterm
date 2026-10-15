//! The terminal state machine: everything a `vte::Perform` callback can change.

use std::collections::VecDeque;
use std::time::Instant;

use bitflags::bitflags;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

use crate::base64;
use crate::cell::{Cell, Flags, Row};
use crate::charset::Charset;
use crate::color::{Color, Palette, Rgb};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Largest DCS payload we buffer. Sixel images will need their own streaming
/// path; this only guards the small query sequences.
const MAX_DCS_LEN: usize = 64 * 1024;
/// The kitty keyboard spec caps the flag stack; the oldest entry is evicted.
const MAX_KEYBOARD_STACK: usize = 16;
const TAB_WIDTH: usize = 8;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct TermMode: u32 {
        const APP_CURSOR = 1 << 0;
        const APP_KEYPAD = 1 << 1;
        const ORIGIN = 1 << 2;
        const AUTOWRAP = 1 << 3;
        const SHOW_CURSOR = 1 << 4;
        const INSERT = 1 << 5;
        const LINEFEED_NEWLINE = 1 << 6;
        const BRACKETED_PASTE = 1 << 7;
        const FOCUS_EVENTS = 1 << 8;
        const MOUSE_X10 = 1 << 9;
        const MOUSE_NORMAL = 1 << 10;
        const MOUSE_BUTTON = 1 << 11;
        const MOUSE_ANY = 1 << 12;
        const MOUSE_SGR = 1 << 13;
        const MOUSE_UTF8 = 1 << 14;
        const MOUSE_URXVT = 1 << 15;
        const ALT_SCREEN = 1 << 16;
        const SYNC_OUTPUT = 1 << 17;
        const ALTERNATE_SCROLL = 1 << 18;
        const CURSOR_BLINK = 1 << 19;
        const MOUSE_SGR_PIXELS = 1 << 20;

        const MOUSE_REPORTING = Self::MOUSE_X10.bits()
            | Self::MOUSE_NORMAL.bits()
            | Self::MOUSE_BUTTON.bits()
            | Self::MOUSE_ANY.bits();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
}

/// A cursor style requested by the application with DECSCUSR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorStyle {
    pub shape: CursorShape,
    pub blinking: bool,
}

/// Something the app has to act on that isn't part of the grid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermEvent {
    Title(String),
    Bell,
    /// OSC 52 store: the text to place on the clipboard.
    ClipboardStore(String),
    /// OSC 7: the shell's working directory, as the `file://` URL it sent.
    Cwd(String),
    /// Mode 2026 ended, so a deferred frame can be drawn now.
    SyncEnd,
}

#[derive(Clone, Debug)]
struct Cursor {
    row: usize,
    col: usize,
    /// The attributes printed characters receive. `c` is unused.
    template: Cell,
    /// The last print filled the final column; the next one wraps first.
    wrap_pending: bool,
}

#[derive(Clone, Debug)]
struct SavedCursor {
    cursor: Cursor,
    origin: bool,
    charsets: [Charset; 4],
    active_charset: usize,
}

enum Dcs {
    /// XTGETTCAP: `DCS + q <hex names> ST`.
    GetTermcap(Vec<u8>),
    /// DECRQSS: `DCS $ q <setting> ST`.
    RequestSetting(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub row: usize,
    pub col: usize,
    /// `None` = the user's configured style.
    pub style: Option<CursorStyle>,
    /// The cursor sits on a double-width character.
    pub wide: bool,
}

/// A copy of what is on screen, taken under the lock so rendering doesn't
/// hold up the parser.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub cols: usize,
    pub rows: usize,
    pub lines: Vec<Row>,
    /// `None` when hidden or scrolled away from the bottom.
    pub cursor: Option<CursorSnapshot>,
    pub palette: Palette,
    pub display_offset: usize,
    pub scrollback_len: usize,
}

/// Feeds PTY output into a [`Term`], keeping parser state between reads so an
/// escape sequence split across two reads still parses.
#[derive(Default)]
pub struct Processor {
    parser: vte::Parser,
}

impl Processor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&mut self, term: &mut Term, bytes: &[u8]) {
        self.parser.advance(term, bytes);
    }
}

pub struct Term {
    cols: usize,
    rows: usize,
    /// The active screen.
    grid: Vec<Row>,
    /// The inactive screen; swapped with `grid` when switching screens.
    other_grid: Vec<Row>,
    scrollback: VecDeque<Row>,
    scrollback_limit: usize,
    /// How many lines the view is scrolled back into the scrollback.
    display_offset: usize,
    cursor: Cursor,
    saved_primary: Option<SavedCursor>,
    saved_alternate: Option<SavedCursor>,
    scroll_top: usize,
    /// Inclusive.
    scroll_bottom: usize,
    tabs: Vec<bool>,
    mode: TermMode,
    charsets: [Charset; 4],
    active_charset: usize,
    title: String,
    title_stack: Vec<String>,
    base_palette: Palette,
    palette: Palette,
    cursor_style: Option<CursorStyle>,
    /// Kitty keyboard flag stacks for the primary and alternate screens.
    keyboard_stacks: [Vec<u8>; 2],
    cell_width: u32,
    cell_height: u32,
    last_printed: Option<char>,
    dcs: Option<Dcs>,
    sync_started: Option<Instant>,
    output: Vec<u8>,
    events: Vec<TermEvent>,
    dirty: bool,
}

impl Term {
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize, palette: Palette) -> Self {
        let cols = cols.max(2);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            grid: blank_grid(cols, rows),
            other_grid: blank_grid(cols, rows),
            scrollback: VecDeque::new(),
            scrollback_limit,
            display_offset: 0,
            cursor: Cursor {
                row: 0,
                col: 0,
                template: Cell::default(),
                wrap_pending: false,
            },
            saved_primary: None,
            saved_alternate: None,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            tabs: default_tabs(cols),
            mode: TermMode::AUTOWRAP | TermMode::SHOW_CURSOR,
            charsets: [Charset::Ascii; 4],
            active_charset: 0,
            title: String::new(),
            title_stack: Vec::new(),
            base_palette: palette.clone(),
            palette,
            cursor_style: None,
            keyboard_stacks: [Vec::new(), Vec::new()],
            cell_width: 0,
            cell_height: 0,
            last_printed: None,
            dcs: None,
            sync_started: None,
            output: Vec::new(),
            events: Vec::new(),
            dirty: true,
        }
    }

    //#region Accessors

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn mode(&self) -> TermMode {
        self.mode
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    /// The active kitty keyboard protocol flags (0 = legacy encoding).
    pub fn keyboard_flags(&self) -> u8 {
        self.keyboard_stack().last().copied().unwrap_or(0)
    }

    /// When synchronized output (mode 2026) began, if it is active. The app
    /// holds frames back while this is set, up to a timeout.
    pub fn sync_started(&self) -> Option<Instant> {
        self.sync_started
    }

    /// Returns whether anything changed since the last call, and clears it.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Bytes the terminal needs to send back to the application (query
    /// replies). The caller writes them to the PTY.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    pub fn take_events(&mut self) -> Vec<TermEvent> {
        std::mem::take(&mut self.events)
    }

    /// Number of lines in the scrollback. Absolute line `n` is scrollback
    /// line `n` below this, and screen row `n - history_len()` above it.
    pub fn history_len(&self) -> usize {
        self.scrollback.len()
    }

    /// The absolute line shown at viewport row `row`.
    pub fn viewport_to_absolute(&self, row: usize) -> usize {
        self.scrollback.len() - self.display_offset + row
    }

    /// A line by absolute index (scrollback first, then the screen).
    pub fn line(&self, absolute: usize) -> Option<&Row> {
        let history = self.scrollback.len();
        if absolute < history {
            self.scrollback.get(absolute)
        } else {
            self.grid.get(absolute - history)
        }
    }

    /// The text between two absolute positions, inclusive. Soft-wrapped lines
    /// are joined without a newline and trailing blanks are dropped.
    pub fn text_between(&self, start: (usize, usize), end: (usize, usize)) -> String {
        let mut out = String::new();
        for line in start.0..=end.0 {
            let Some(row) = self.line(line) else { break };
            let from = if line == start.0 { start.1 } else { 0 };
            let to = if line == end.0 { (end.1 + 1).min(row.cells.len()) } else { row.cells.len() };
            let mut text = String::new();
            for cell in row.cells.get(from..to).unwrap_or_default() {
                if !cell.flags.contains(Flags::WIDE_SPACER) {
                    text.push_str(&cell.text());
                }
            }
            let wraps = row.wrapped && line != end.0;
            if wraps {
                out.push_str(&text);
            } else {
                out.push_str(text.trim_end());
                if line != end.0 {
                    out.push('\n');
                }
            }
        }
        out
    }

    //#endregion

    //#region App-side controls

    /// Replaces the theme palette. Colours the application changed with OSC 4/10/11/12
    /// are dropped, so a theme switch always takes full effect.
    pub fn set_palette(&mut self, palette: Palette) {
        self.base_palette = palette.clone();
        self.palette = palette;
        self.dirty = true;
    }

    pub fn set_scrollback_limit(&mut self, limit: usize) {
        self.scrollback_limit = limit;
        self.trim_scrollback();
    }

    /// The pixel size of one cell, reported to applications through `CSI 14/16 t`.
    pub fn set_cell_size(&mut self, width: u32, height: u32) {
        self.cell_width = width;
        self.cell_height = height;
    }

    /// Scrolls the view by `delta` lines; positive scrolls back into history.
    pub fn scroll_display(&mut self, delta: isize) {
        let max = self.scrollback.len() as isize;
        let next = (self.display_offset as isize + delta).clamp(0, max) as usize;
        if next != self.display_offset {
            self.display_offset = next;
            self.dirty = true;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_display(-(self.display_offset as isize));
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let alt_active = self.mode.contains(TermMode::ALT_SCREEN);

        // Columns: no reflow yet, lines are cut or padded.
        for row in self
            .grid
            .iter_mut()
            .chain(self.other_grid.iter_mut())
            .chain(self.scrollback.iter_mut())
        {
            row.resize(cols);
        }

        // Rows. On the primary screen, lines leaving the top go to the
        // scrollback, and growing pulls them back, like most terminals do.
        let (primary, alternate) = if alt_active {
            (&mut self.other_grid, &mut self.grid)
        } else {
            (&mut self.grid, &mut self.other_grid)
        };
        let mut cursor_shift: isize = 0;
        if rows < self.rows {
            let cursor_row = if alt_active { 0 } else { self.cursor.row };
            let overflow = (cursor_row + 1).saturating_sub(rows);
            for row in primary.drain(..overflow) {
                self.scrollback.push_back(row);
            }
            primary.truncate(rows);
            if !alt_active {
                cursor_shift = -(overflow as isize);
            }
            let alt_overflow = if alt_active {
                (self.cursor.row + 1).saturating_sub(rows)
            } else {
                0
            };
            alternate.drain(..alt_overflow);
            alternate.truncate(rows);
            if alt_active {
                cursor_shift = -(alt_overflow as isize);
            }
        } else {
            let pulled = if alt_active {
                0
            } else {
                (rows - self.rows).min(self.scrollback.len())
            };
            for _ in 0..pulled {
                let row = self.scrollback.pop_back().expect("checked length");
                primary.insert(0, row);
            }
            cursor_shift = pulled as isize;
            primary.resize(rows, Row::new(cols, Color::Default));
            alternate.resize(rows, Row::new(cols, Color::Default));
        }

        self.cols = cols;
        self.rows = rows;
        self.cursor.row = (self.cursor.row as isize + cursor_shift).clamp(0, rows as isize - 1) as usize;
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.wrap_pending = false;
        for saved in [&mut self.saved_primary, &mut self.saved_alternate].into_iter().flatten() {
            saved.cursor.row = saved.cursor.row.min(rows - 1);
            saved.cursor.col = saved.cursor.col.min(cols - 1);
        }
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.tabs = default_tabs(cols);
        self.trim_scrollback();
        self.display_offset = self.display_offset.min(self.scrollback.len());
        self.dirty = true;
    }

    pub fn snapshot(&self) -> Snapshot {
        let offset = self.display_offset.min(self.scrollback.len());
        let from_history = offset.min(self.rows);
        let mut lines = Vec::with_capacity(self.rows);
        let start = self.scrollback.len() - offset;
        lines.extend(self.scrollback.range(start..start + from_history).cloned());
        lines.extend(self.grid[..self.rows - from_history].iter().cloned());

        let cursor = (offset == 0 && self.mode.contains(TermMode::SHOW_CURSOR)).then(|| {
            let cell = &self.grid[self.cursor.row].cells[self.cursor.col];
            CursorSnapshot {
                row: self.cursor.row,
                col: self.cursor.col,
                style: self.cursor_style,
                wide: cell.flags.contains(Flags::WIDE),
            }
        });

        Snapshot {
            cols: self.cols,
            rows: self.rows,
            lines,
            cursor,
            palette: self.palette.clone(),
            display_offset: offset,
            scrollback_len: self.scrollback.len(),
        }
    }

    //#endregion

    //#region Internal helpers

    fn keyboard_stack(&self) -> &Vec<u8> {
        &self.keyboard_stacks[usize::from(self.mode.contains(TermMode::ALT_SCREEN))]
    }

    fn keyboard_stack_mut(&mut self) -> &mut Vec<u8> {
        &mut self.keyboard_stacks[usize::from(self.mode.contains(TermMode::ALT_SCREEN))]
    }

    fn blank(&self) -> Cell {
        Cell::blank(self.cursor.template.bg)
    }

    fn blank_row(&self) -> Row {
        Row::new(self.cols, self.cursor.template.bg)
    }

    fn reply(&mut self, s: &str) {
        self.output.extend_from_slice(s.as_bytes());
    }

    fn trim_scrollback(&mut self) {
        while self.scrollback.len() > self.scrollback_limit {
            self.scrollback.pop_front();
        }
    }

    /// Scrolls lines `top..=bottom` up by `n`. With `save`, lines leaving the
    /// top of the primary screen go into the scrollback.
    fn scroll_up_in(&mut self, top: usize, bottom: usize, n: usize, save: bool) {
        let n = n.min(bottom + 1 - top);
        let save = save && top == 0 && !self.mode.contains(TermMode::ALT_SCREEN);
        for _ in 0..n {
            let row = self.grid.remove(top);
            if save {
                self.scrollback.push_back(row);
                // Keep a scrolled-back view still while output arrives.
                if self.display_offset > 0 {
                    self.display_offset += 1;
                }
            }
            self.grid.insert(bottom, self.blank_row());
        }
        if save {
            self.trim_scrollback();
            self.display_offset = self.display_offset.min(self.scrollback.len());
        }
        self.dirty = true;
    }

    fn scroll_down_in(&mut self, top: usize, bottom: usize, n: usize) {
        let n = n.min(bottom + 1 - top);
        for _ in 0..n {
            self.grid.remove(bottom);
            self.grid.insert(top, self.blank_row());
        }
        self.dirty = true;
    }

    fn linefeed(&mut self) {
        self.cursor.wrap_pending = false;
        if self.cursor.row == self.scroll_bottom {
            self.scroll_up_in(self.scroll_top, self.scroll_bottom, 1, true);
        } else if self.cursor.row < self.rows - 1 {
            self.cursor.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.cursor.wrap_pending = false;
        if self.cursor.row == self.scroll_top {
            self.scroll_down_in(self.scroll_top, self.scroll_bottom, 1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    /// Moves to an absolute position. `row` is relative to the scroll region
    /// when origin mode is on.
    fn goto(&mut self, row: usize, col: usize) {
        let (min, max) = if self.mode.contains(TermMode::ORIGIN) {
            (self.scroll_top, self.scroll_bottom)
        } else {
            (0, self.rows - 1)
        };
        self.cursor.row = (row + min).min(max);
        self.cursor.col = col.min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    fn goto_row(&mut self, row: usize) {
        self.goto(row, self.cursor.col);
    }

    fn goto_col(&mut self, col: usize) {
        self.cursor.col = col.min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    /// Moves vertically by `delta`, stopping at the scroll region's edge if
    /// the cursor starts inside it.
    fn move_vertical(&mut self, delta: isize) {
        let row = self.cursor.row as isize;
        let (top, bottom) = if (self.scroll_top..=self.scroll_bottom).contains(&self.cursor.row) {
            (self.scroll_top as isize, self.scroll_bottom as isize)
        } else {
            (0, self.rows as isize - 1)
        };
        self.cursor.row = (row + delta).clamp(top, bottom) as usize;
        self.cursor.wrap_pending = false;
    }

    /// Clears whichever half of a double-width character is left behind when
    /// the other half at `col` gets overwritten.
    fn fix_wide_fragments(&mut self, row: usize, col: usize) {
        let cells = &mut self.grid[row].cells;
        let flags = cells[col].flags;
        if flags.contains(Flags::WIDE_SPACER) && col > 0 {
            cells[col - 1] = Cell::blank(cells[col - 1].bg);
        }
        if flags.contains(Flags::WIDE) && col + 1 < cells.len() {
            cells[col + 1] = Cell::blank(cells[col + 1].bg);
        }
    }

    fn erase_cells(&mut self, row: usize, range: std::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.fix_wide_fragments(row, range.start);
        self.fix_wide_fragments(row, range.end - 1);
        let blank = self.blank();
        for cell in &mut self.grid[row].cells[range] {
            *cell = blank.clone();
        }
        self.dirty = true;
    }

    fn erase_rows(&mut self, rows: std::ops::Range<usize>) {
        for row in rows {
            self.grid[row] = self.blank_row();
        }
        self.dirty = true;
    }

    fn save_cursor(&mut self) {
        let saved = SavedCursor {
            cursor: self.cursor.clone(),
            origin: self.mode.contains(TermMode::ORIGIN),
            charsets: self.charsets,
            active_charset: self.active_charset,
        };
        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.saved_alternate = Some(saved);
        } else {
            self.saved_primary = Some(saved);
        }
    }

    fn restore_cursor(&mut self) {
        let saved = if self.mode.contains(TermMode::ALT_SCREEN) {
            self.saved_alternate.clone()
        } else {
            self.saved_primary.clone()
        };
        match saved {
            Some(saved) => {
                self.cursor = saved.cursor;
                self.cursor.row = self.cursor.row.min(self.rows - 1);
                self.cursor.col = self.cursor.col.min(self.cols - 1);
                self.mode.set(TermMode::ORIGIN, saved.origin);
                self.charsets = saved.charsets;
                self.active_charset = saved.active_charset;
            }
            None => {
                self.cursor.row = 0;
                self.cursor.col = 0;
                self.cursor.template = Cell::default();
                self.cursor.wrap_pending = false;
                self.mode.remove(TermMode::ORIGIN);
            }
        }
    }

    fn enter_alt_screen(&mut self, save_cursor: bool, clear: bool) {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            return;
        }
        if save_cursor {
            self.save_cursor();
        }
        std::mem::swap(&mut self.grid, &mut self.other_grid);
        self.mode.insert(TermMode::ALT_SCREEN);
        if clear {
            self.erase_rows(0..self.rows);
        }
        self.display_offset = 0;
        self.dirty = true;
    }

    fn leave_alt_screen(&mut self, restore_cursor: bool, clear: bool) {
        if !self.mode.contains(TermMode::ALT_SCREEN) {
            return;
        }
        if clear {
            self.erase_rows(0..self.rows);
        }
        std::mem::swap(&mut self.grid, &mut self.other_grid);
        self.mode.remove(TermMode::ALT_SCREEN);
        if restore_cursor {
            self.restore_cursor();
        }
        self.dirty = true;
    }

    fn reset(&mut self) {
        let mut fresh = Term::new(self.cols, self.rows, self.scrollback_limit, self.base_palette.clone());
        fresh.cell_width = self.cell_width;
        fresh.cell_height = self.cell_height;
        fresh.output = std::mem::take(&mut self.output);
        fresh.events = std::mem::take(&mut self.events);
        *self = fresh;
    }

    //#endregion

    //#region Modes

    fn set_private_mode(&mut self, mode: u16, on: bool) {
        let flag = match mode {
            1 => TermMode::APP_CURSOR,
            6 => {
                self.mode.set(TermMode::ORIGIN, on);
                self.goto(0, 0);
                return;
            }
            7 => TermMode::AUTOWRAP,
            9 => TermMode::MOUSE_X10,
            12 => TermMode::CURSOR_BLINK,
            25 => TermMode::SHOW_CURSOR,
            66 => TermMode::APP_KEYPAD,
            1000 => TermMode::MOUSE_NORMAL,
            1002 => TermMode::MOUSE_BUTTON,
            1003 => TermMode::MOUSE_ANY,
            1004 => TermMode::FOCUS_EVENTS,
            1005 => TermMode::MOUSE_UTF8,
            1006 => TermMode::MOUSE_SGR,
            1007 => TermMode::ALTERNATE_SCROLL,
            1015 => TermMode::MOUSE_URXVT,
            1016 => TermMode::MOUSE_SGR_PIXELS,
            2004 => TermMode::BRACKETED_PASTE,
            2026 => {
                if on {
                    self.sync_started.get_or_insert_with(Instant::now);
                } else if self.sync_started.take().is_some() {
                    self.events.push(TermEvent::SyncEnd);
                }
                self.mode.set(TermMode::SYNC_OUTPUT, on);
                return;
            }
            47 => {
                if on {
                    self.enter_alt_screen(false, false)
                } else {
                    self.leave_alt_screen(false, false)
                }
                return;
            }
            1047 => {
                if on {
                    self.enter_alt_screen(false, false)
                } else {
                    self.leave_alt_screen(false, true)
                }
                return;
            }
            1048 => {
                if on {
                    self.save_cursor()
                } else {
                    self.restore_cursor()
                }
                return;
            }
            1049 => {
                if on {
                    self.enter_alt_screen(true, true)
                } else {
                    self.leave_alt_screen(true, false)
                }
                return;
            }
            _ => {
                tracing::debug!("unhandled private mode {mode} = {on}");
                return;
            }
        };
        // Mouse tracking modes are mutually exclusive.
        if on && TermMode::MOUSE_REPORTING.contains(flag) {
            self.mode.remove(TermMode::MOUSE_REPORTING);
        }
        self.mode.set(flag, on);
        self.dirty = true;
    }

    /// DECRQM answer: 1 = set, 2 = reset, 0 = not recognised.
    fn private_mode_state(&self, mode: u16) -> u8 {
        let flag = match mode {
            1 => TermMode::APP_CURSOR,
            6 => TermMode::ORIGIN,
            7 => TermMode::AUTOWRAP,
            9 => TermMode::MOUSE_X10,
            12 => TermMode::CURSOR_BLINK,
            25 => TermMode::SHOW_CURSOR,
            66 => TermMode::APP_KEYPAD,
            1000 => TermMode::MOUSE_NORMAL,
            1002 => TermMode::MOUSE_BUTTON,
            1003 => TermMode::MOUSE_ANY,
            1004 => TermMode::FOCUS_EVENTS,
            1005 => TermMode::MOUSE_UTF8,
            1006 => TermMode::MOUSE_SGR,
            1007 => TermMode::ALTERNATE_SCROLL,
            1015 => TermMode::MOUSE_URXVT,
            1016 => TermMode::MOUSE_SGR_PIXELS,
            47 | 1047 | 1049 => TermMode::ALT_SCREEN,
            2004 => TermMode::BRACKETED_PASTE,
            2026 => TermMode::SYNC_OUTPUT,
            _ => return 0,
        };
        if self.mode.contains(flag) { 1 } else { 2 }
    }

    //#endregion

    //#region SGR

    fn sgr(&mut self, params: &Params) {
        let params: Vec<&[u16]> = params.iter().collect();
        if params.is_empty() {
            self.cursor.template = Cell::default();
            return;
        }
        let t = &mut self.cursor.template;
        let mut i = 0;
        while i < params.len() {
            let p = params[i];
            match p[0] {
                0 => *t = Cell::default(),
                1 => t.flags.insert(Flags::BOLD),
                2 => t.flags.insert(Flags::DIM),
                3 => t.flags.insert(Flags::ITALIC),
                4 => {
                    t.flags.remove(Flags::ALL_UNDERLINES);
                    // 4:x selects an underline style.
                    match p.get(1).copied().unwrap_or(1) {
                        0 => {}
                        2 => t.flags.insert(Flags::DOUBLE_UNDERLINE),
                        3 => t.flags.insert(Flags::CURLY_UNDERLINE),
                        4 => t.flags.insert(Flags::DOTTED_UNDERLINE),
                        5 => t.flags.insert(Flags::DASHED_UNDERLINE),
                        _ => t.flags.insert(Flags::UNDERLINE),
                    }
                }
                5 | 6 => t.flags.insert(Flags::BLINK),
                7 => t.flags.insert(Flags::INVERSE),
                8 => t.flags.insert(Flags::HIDDEN),
                9 => t.flags.insert(Flags::STRIKE),
                21 => {
                    t.flags.remove(Flags::ALL_UNDERLINES);
                    t.flags.insert(Flags::DOUBLE_UNDERLINE);
                }
                22 => t.flags.remove(Flags::BOLD | Flags::DIM),
                23 => t.flags.remove(Flags::ITALIC),
                24 => t.flags.remove(Flags::ALL_UNDERLINES),
                25 => t.flags.remove(Flags::BLINK),
                27 => t.flags.remove(Flags::INVERSE),
                28 => t.flags.remove(Flags::HIDDEN),
                29 => t.flags.remove(Flags::STRIKE),
                n @ 30..=37 => t.fg = Color::Indexed((n - 30) as u8),
                38 => {
                    if let Some(color) = parse_extended_color(&params, &mut i) {
                        t.fg = color;
                    }
                }
                39 => t.fg = Color::Default,
                n @ 40..=47 => t.bg = Color::Indexed((n - 40) as u8),
                48 => {
                    if let Some(color) = parse_extended_color(&params, &mut i) {
                        t.bg = color;
                    }
                }
                49 => t.bg = Color::Default,
                58 => {
                    if let Some(color) = parse_extended_color(&params, &mut i) {
                        t.underline_color = color;
                    }
                }
                59 => t.underline_color = Color::Default,
                n @ 90..=97 => t.fg = Color::Indexed((n - 90 + 8) as u8),
                n @ 100..=107 => t.bg = Color::Indexed((n - 100 + 8) as u8),
                n => tracing::debug!("unhandled SGR {n}"),
            }
            i += 1;
        }
    }

    //#endregion

    //#region OSC

    fn osc_color_reply(&mut self, code: &str, color: Rgb, bell: bool) {
        let terminator = if bell { "\x07" } else { "\x1b\\" };
        self.reply(&format!("\x1b]{code};{}{terminator}", color.to_spec()));
    }

    /// OSC 10/11/12 take one spec per parameter, starting at their own code
    /// and continuing to the next: `OSC 10;?;? ST` queries 10 and 11.
    fn osc_dynamic_colors(&mut self, first: u16, specs: &[&[u8]], bell: bool) {
        for (offset, spec) in specs.iter().enumerate() {
            let code = first + offset as u16;
            let spec = String::from_utf8_lossy(spec);
            let target = match code {
                10 => &mut self.palette.foreground,
                11 => &mut self.palette.background,
                12 => &mut self.palette.cursor,
                _ => break,
            };
            if spec == "?" {
                let color = *target;
                self.osc_color_reply(&code.to_string(), color, bell);
            } else if let Some(color) = Rgb::parse_spec(&spec) {
                *target = color;
                self.dirty = true;
            }
        }
    }

    //#endregion

    //#region DCS

    fn finish_dcs(&mut self, dcs: Dcs) {
        match dcs {
            Dcs::GetTermcap(data) => {
                for name_hex in data.split(|&b| b == b';') {
                    let Some(name) = hex_decode(name_hex) else {
                        continue;
                    };
                    let hex_name = String::from_utf8_lossy(name_hex).into_owned();
                    match termcap_value(&String::from_utf8_lossy(&name)) {
                        Some(Some(value)) => {
                            self.reply(&format!("\x1bP1+r{hex_name}={}\x1b\\", hex_encode(value.as_bytes())))
                        }
                        Some(None) => self.reply(&format!("\x1bP1+r{hex_name}\x1b\\")),
                        None => self.reply(&format!("\x1bP0+r{hex_name}\x1b\\")),
                    }
                }
            }
            Dcs::RequestSetting(data) => {
                let reply = match data.as_slice() {
                    b"r" => Some(format!("{};{}r", self.scroll_top + 1, self.scroll_bottom + 1)),
                    b" q" => {
                        let n = match self.cursor_style {
                            None => 0,
                            Some(style) => {
                                let base = match style.shape {
                                    CursorShape::Block => 1,
                                    CursorShape::Underline => 3,
                                    CursorShape::Beam => 5,
                                };
                                base + u8::from(!style.blinking)
                            }
                        };
                        Some(format!("{n} q"))
                    }
                    b"m" => Some(sgr_string(&self.cursor.template)),
                    _ => None,
                };
                match reply {
                    Some(value) => self.reply(&format!("\x1bP1$r{value}\x1b\\")),
                    None => self.reply("\x1bP0$r\x1b\\"),
                }
            }
        }
    }

    //#endregion
}

impl Perform for Term {
    fn print(&mut self, c: char) {
        let c = self.charsets[self.active_charset].map(c);
        let Some(width) = c.width() else {
            return;
        };
        self.dirty = true;

        if width == 0 {
            // Combining character: attach it to the previous cell.
            let row = self.cursor.row;
            let mut col = if self.cursor.wrap_pending {
                self.cursor.col
            } else if self.cursor.col > 0 {
                self.cursor.col - 1
            } else {
                return;
            };
            if self.grid[row].cells[col].flags.contains(Flags::WIDE_SPACER) && col > 0 {
                col -= 1;
            }
            let cell = &mut self.grid[row].cells[col];
            let mut extra = cell.extra.take().map(String::from).unwrap_or_default();
            extra.push(c);
            cell.extra = Some(extra.into_boxed_str());
            return;
        }

        let autowrap = self.mode.contains(TermMode::AUTOWRAP);
        if self.cursor.wrap_pending && autowrap {
            self.grid[self.cursor.row].wrapped = true;
            self.linefeed();
            self.cursor.col = 0;
        }
        if width == 2 && self.cursor.col + 1 >= self.cols {
            if autowrap {
                self.grid[self.cursor.row].wrapped = true;
                self.linefeed();
                self.cursor.col = 0;
            } else {
                self.cursor.col = self.cols - 2;
            }
        }

        let (row, col) = (self.cursor.row, self.cursor.col);
        if self.mode.contains(TermMode::INSERT) {
            let blank = self.blank();
            let cells = &mut self.grid[row].cells;
            for _ in 0..width {
                cells.insert(col, blank.clone());
                cells.pop();
            }
        }

        self.fix_wide_fragments(row, col);
        if width == 2 {
            self.fix_wide_fragments(row, col + 1);
        }
        let mut cell = self.cursor.template.clone();
        cell.c = c;
        cell.extra = None;
        cell.flags.remove(Flags::WIDE | Flags::WIDE_SPACER);
        if width == 2 {
            cell.flags.insert(Flags::WIDE);
            let mut spacer = cell.clone();
            spacer.c = ' ';
            spacer.flags.remove(Flags::WIDE);
            spacer.flags.insert(Flags::WIDE_SPACER);
            self.grid[row].cells[col + 1] = spacer;
        }
        self.grid[row].cells[col] = cell;
        self.last_printed = Some(c);

        self.cursor.col += width;
        if self.cursor.col >= self.cols {
            self.cursor.col = self.cols - 1;
            self.cursor.wrap_pending = autowrap;
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.events.push(TermEvent::Bell),
            0x08 => {
                self.cursor.col = self.cursor.col.saturating_sub(1);
                self.cursor.wrap_pending = false;
            }
            0x09 => {
                let next = (self.cursor.col + 1..self.cols).find(|&c| self.tabs[c]);
                self.goto_col(next.unwrap_or(self.cols - 1));
            }
            0x0a..=0x0c => {
                self.linefeed();
                if self.mode.contains(TermMode::LINEFEED_NEWLINE) {
                    self.cursor.col = 0;
                }
            }
            0x0d => {
                self.cursor.col = 0;
                self.cursor.wrap_pending = false;
            }
            0x0e => self.active_charset = 1,
            0x0f => self.active_charset = 0,
            _ => tracing::debug!("unhandled control {byte:#04x}"),
        }
    }

    fn hook(&mut self, _params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.dcs = match (intermediates, action) {
            ([b'+'], 'q') => Some(Dcs::GetTermcap(Vec::new())),
            ([b'$'], 'q') => Some(Dcs::RequestSetting(Vec::new())),
            _ => None,
        };
    }

    fn put(&mut self, byte: u8) {
        if let Some(Dcs::GetTermcap(data) | Dcs::RequestSetting(data)) = &mut self.dcs
            && data.len() < MAX_DCS_LEN
        {
            data.push(byte);
        }
    }

    fn unhook(&mut self) {
        if let Some(dcs) = self.dcs.take() {
            self.finish_dcs(dcs);
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let Some(code) = params.first().and_then(|p| std::str::from_utf8(p).ok()) else {
            return;
        };
        match code {
            "0" | "2" => {
                // Titles may contain `;`, which split them into several params.
                let title = params[1..]
                    .iter()
                    .map(|p| String::from_utf8_lossy(p))
                    .collect::<Vec<_>>()
                    .join(";");
                self.title = title.clone();
                self.events.push(TermEvent::Title(title));
            }
            "1" => {}
            "4" => {
                for pair in params[1..].chunks(2) {
                    let [index, spec] = pair else { break };
                    let Some(index) = std::str::from_utf8(index).ok().and_then(|s| s.parse::<u8>().ok()) else {
                        continue;
                    };
                    if *spec == b"?" {
                        let color = self.palette.colors[index as usize];
                        self.osc_color_reply(&format!("4;{index}"), color, bell_terminated);
                    } else if let Some(color) = Rgb::parse_spec(&String::from_utf8_lossy(spec)) {
                        self.palette.colors[index as usize] = color;
                        self.dirty = true;
                    }
                }
            }
            "7" => {
                if let Some(url) = params.get(1) {
                    self.events.push(TermEvent::Cwd(String::from_utf8_lossy(url).into_owned()));
                }
            }
            "10" | "11" | "12" => {
                let first = code.parse().expect("matched digits");
                self.osc_dynamic_colors(first, &params[1..], bell_terminated);
            }
            "52" => {
                // OSC 52 ; <selection> ; <base64 | ?>. Reading the clipboard
                // back is not allowed: any program could exfiltrate it.
                if let Some(data) = params.get(2)
                    && *data != b"?"
                    && let Some(bytes) = base64::decode(data)
                {
                    self.events
                        .push(TermEvent::ClipboardStore(String::from_utf8_lossy(&bytes).into_owned()));
                }
            }
            "104" => {
                if params.len() <= 1 || params[1].is_empty() {
                    self.palette.colors = self.base_palette.colors;
                } else {
                    for index in &params[1..] {
                        if let Some(i) = std::str::from_utf8(index).ok().and_then(|s| s.parse::<u8>().ok()) {
                            self.palette.colors[i as usize] = self.base_palette.colors[i as usize];
                        }
                    }
                }
                self.dirty = true;
            }
            "110" => self.palette.foreground = self.base_palette.foreground,
            "111" => self.palette.background = self.base_palette.background,
            "112" => self.palette.cursor = self.base_palette.cursor,
            _ => tracing::debug!("unhandled OSC {code}"),
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // First value of each parameter; 0 means "omitted".
        let args: Vec<u16> = params.iter().map(|p| p[0]).collect();
        let arg = |i: usize, default: u16| -> u16 {
            match args.get(i).copied() {
                Some(0) | None => default,
                Some(v) => v,
            }
        };
        let n = |i: usize| arg(i, 1) as usize;
        self.dirty = true;

        match (intermediates, action) {
            ([], '@') => {
                let (row, col) = (self.cursor.row, self.cursor.col);
                let count = n(0).min(self.cols - col);
                self.fix_wide_fragments(row, col);
                let blank = self.blank();
                let cells = &mut self.grid[row].cells;
                for _ in 0..count {
                    cells.insert(col, blank.clone());
                    cells.pop();
                }
            }
            ([], 'A') => self.move_vertical(-(n(0) as isize)),
            ([], 'B') | ([], 'e') => self.move_vertical(n(0) as isize),
            ([], 'C') | ([], 'a') => self.goto_col(self.cursor.col + n(0)),
            ([], 'D') => self.goto_col(self.cursor.col.saturating_sub(n(0))),
            ([], 'E') => {
                self.move_vertical(n(0) as isize);
                self.cursor.col = 0;
            }
            ([], 'F') => {
                self.move_vertical(-(n(0) as isize));
                self.cursor.col = 0;
            }
            ([], 'G') | ([], '`') => self.goto_col(n(0) - 1),
            ([], 'H') | ([], 'f') => self.goto(n(0) - 1, n(1) - 1),
            ([], 'I') => {
                for _ in 0..n(0) {
                    self.execute(0x09);
                }
            }
            ([], 'J') | ([b'?'], 'J') => {
                let (row, col) = (self.cursor.row, self.cursor.col);
                match arg(0, 0) {
                    0 => {
                        self.erase_cells(row, col..self.cols);
                        self.erase_rows(row + 1..self.rows);
                    }
                    1 => {
                        self.erase_rows(0..row);
                        self.erase_cells(row, 0..col + 1);
                    }
                    2 => self.erase_rows(0..self.rows),
                    3 => {
                        self.scrollback.clear();
                        self.display_offset = 0;
                    }
                    _ => {}
                }
            }
            ([], 'K') | ([b'?'], 'K') => {
                let (row, col) = (self.cursor.row, self.cursor.col);
                match arg(0, 0) {
                    0 => self.erase_cells(row, col..self.cols),
                    1 => self.erase_cells(row, 0..col + 1),
                    2 => self.erase_cells(row, 0..self.cols),
                    _ => {}
                }
            }
            ([], 'L') => {
                if (self.scroll_top..=self.scroll_bottom).contains(&self.cursor.row) {
                    self.scroll_down_in(self.cursor.row, self.scroll_bottom, n(0));
                    self.cursor.col = 0;
                }
            }
            ([], 'M') => {
                if (self.scroll_top..=self.scroll_bottom).contains(&self.cursor.row) {
                    self.scroll_up_in(self.cursor.row, self.scroll_bottom, n(0), false);
                    self.cursor.col = 0;
                }
            }
            ([], 'P') => {
                let (row, col) = (self.cursor.row, self.cursor.col);
                let count = n(0).min(self.cols - col);
                self.fix_wide_fragments(row, col);
                let blank = self.blank();
                let cells = &mut self.grid[row].cells;
                cells.drain(col..col + count);
                cells.resize(self.cols, blank);
            }
            ([], 'S') => self.scroll_up_in(self.scroll_top, self.scroll_bottom, n(0), true),
            ([], 'T') => self.scroll_down_in(self.scroll_top, self.scroll_bottom, n(0)),
            ([], 'X') => {
                let col = self.cursor.col;
                let end = (col + n(0)).min(self.cols);
                self.erase_cells(self.cursor.row, col..end);
            }
            ([], 'Z') => {
                for _ in 0..n(0) {
                    let prev = (0..self.cursor.col).rev().find(|&c| self.tabs[c]);
                    self.goto_col(prev.unwrap_or(0));
                }
            }
            ([], 'b') => {
                if let Some(c) = self.last_printed {
                    for _ in 0..n(0) {
                        self.print(c);
                    }
                }
            }
            ([], 'c') if arg(0, 0) == 0 => {
                // VT220 with ANSI colour and clipboard access.
                self.reply("\x1b[?62;22;52c");
            }
            ([b'>'], 'c') if arg(0, 0) == 0 => {
                let version: u32 = VERSION
                    .split('.')
                    .take(3)
                    .fold(0, |acc, part| acc * 100 + part.parse::<u32>().unwrap_or(0));
                self.reply(&format!("\x1b[>1;{version};0c"));
            }
            ([], 'd') => self.goto_row(n(0) - 1),
            ([], 'g') => match arg(0, 0) {
                0 => self.tabs[self.cursor.col] = false,
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {}
            },
            ([], 'h') | ([], 'l') => {
                let on = action == 'h';
                for &mode in &args {
                    match mode {
                        4 => self.mode.set(TermMode::INSERT, on),
                        20 => self.mode.set(TermMode::LINEFEED_NEWLINE, on),
                        _ => tracing::debug!("unhandled ANSI mode {mode}"),
                    }
                }
            }
            ([b'?'], 'h') | ([b'?'], 'l') => {
                for &mode in &args {
                    self.set_private_mode(mode, action == 'h');
                }
            }
            ([], 'm') => self.sgr(params),
            ([], 'n') => match arg(0, 0) {
                5 => self.reply("\x1b[0n"),
                6 => {
                    let row = if self.mode.contains(TermMode::ORIGIN) {
                        self.cursor.row - self.scroll_top
                    } else {
                        self.cursor.row
                    };
                    self.reply(&format!("\x1b[{};{}R", row + 1, self.cursor.col + 1));
                }
                _ => {}
            },
            ([b'?'], 'n') if arg(0, 0) == 6 => {
                self.reply(&format!("\x1b[?{};{}R", self.cursor.row + 1, self.cursor.col + 1));
            }
            ([b'$'], 'p') => {
                let mode = arg(0, 0);
                let state = match mode {
                    4 => if self.mode.contains(TermMode::INSERT) { 1 } else { 2 },
                    20 => if self.mode.contains(TermMode::LINEFEED_NEWLINE) { 1 } else { 2 },
                    _ => 0,
                };
                self.reply(&format!("\x1b[{mode};{state}$y"));
            }
            ([b'?', b'$'], 'p') => {
                let mode = arg(0, 0);
                let state = self.private_mode_state(mode);
                self.reply(&format!("\x1b[?{mode};{state}$y"));
            }
            ([b' '], 'q') => {
                self.cursor_style = match arg(0, 0) {
                    0 => None,
                    n => {
                        let shape = match n {
                            1 | 2 => CursorShape::Block,
                            3 | 4 => CursorShape::Underline,
                            _ => CursorShape::Beam,
                        };
                        Some(CursorStyle {
                            shape,
                            blinking: n % 2 == 1,
                        })
                    }
                };
            }
            ([b'>'], 'q') => self.reply(&format!("\x1bP>|ionterm({VERSION})\x1b\\")),
            ([], 'r') => {
                let top = arg(0, 1) as usize - 1;
                let bottom = (arg(1, self.rows as u16) as usize).min(self.rows) - 1;
                if top < bottom {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                    self.goto(0, 0);
                }
            }
            ([], 's') => self.save_cursor(),
            ([], 't') => match arg(0, 0) {
                14 => {
                    let (h, w) = (self.cell_height * self.rows as u32, self.cell_width * self.cols as u32);
                    self.reply(&format!("\x1b[4;{h};{w}t"));
                }
                16 => {
                    let (h, w) = (self.cell_height, self.cell_width);
                    self.reply(&format!("\x1b[6;{h};{w}t"));
                }
                18 => {
                    let (rows, cols) = (self.rows, self.cols);
                    self.reply(&format!("\x1b[8;{rows};{cols}t"));
                }
                22 => self.title_stack.push(self.title.clone()),
                23 => {
                    if let Some(title) = self.title_stack.pop() {
                        self.title = title.clone();
                        self.events.push(TermEvent::Title(title));
                    }
                }
                _ => {}
            },
            ([], 'u') => self.restore_cursor(),
            // Kitty keyboard protocol.
            ([b'?'], 'u') => {
                let flags = self.keyboard_flags();
                self.reply(&format!("\x1b[?{flags}u"));
            }
            ([b'>'], 'u') => {
                let flags = arg(0, 0) as u8;
                let stack = self.keyboard_stack_mut();
                if stack.len() >= MAX_KEYBOARD_STACK {
                    stack.remove(0);
                }
                stack.push(flags);
            }
            ([b'<'], 'u') => {
                let count = n(0);
                let stack = self.keyboard_stack_mut();
                let keep = stack.len().saturating_sub(count);
                stack.truncate(keep);
            }
            ([b'='], 'u') => {
                let flags = arg(0, 0) as u8;
                let current = self.keyboard_flags();
                let next = match arg(1, 1) {
                    2 => current | flags,
                    3 => current & !flags,
                    _ => flags,
                };
                let stack = self.keyboard_stack_mut();
                match stack.last_mut() {
                    Some(top) => *top = next,
                    None => stack.push(next),
                }
            }
            _ => tracing::debug!("unhandled CSI {:?} {action} {args:?}", String::from_utf8_lossy(intermediates)),
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.dirty = true;
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([b'#'], b'8') => {
                // DECALN: fill the screen with `E` for alignment tests.
                for row in &mut self.grid {
                    for cell in &mut row.cells {
                        *cell = Cell {
                            c: 'E',
                            ..Cell::default()
                        };
                    }
                }
            }
            ([], b'D') => self.linefeed(),
            ([], b'E') => {
                self.linefeed();
                self.cursor.col = 0;
            }
            ([], b'H') => self.tabs[self.cursor.col] = true,
            ([], b'M') => self.reverse_index(),
            ([], b'c') => self.reset(),
            ([], b'=') => self.mode.insert(TermMode::APP_KEYPAD),
            ([], b'>') => self.mode.remove(TermMode::APP_KEYPAD),
            ([b'('], b) | ([b')'], b) | ([b'*'], b) | ([b'+'], b) => {
                let slot = (intermediates[0] - b'(') as usize;
                if let Some(charset) = Charset::from_designator(b) {
                    self.charsets[slot] = charset;
                }
            }
            ([], b'\\') => {}
            _ => tracing::debug!("unhandled ESC {:?} {}", String::from_utf8_lossy(intermediates), byte as char),
        }
    }
}

fn blank_grid(cols: usize, rows: usize) -> Vec<Row> {
    vec![Row::new(cols, Color::Default); rows]
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|c| c > 0 && c % TAB_WIDTH == 0).collect()
}

/// Parses the colour after SGR 38/48/58, in either the colon form
/// (`38:2::r:g:b`, `38:5:n`) or the legacy semicolon form (`38;2;r;g;b`).
/// Advances `i` past any extra parameters the semicolon form consumed.
fn parse_extended_color(params: &[&[u16]], i: &mut usize) -> Option<Color> {
    let p = params[*i];
    if p.len() > 1 {
        return match p[1] {
            5 => p.get(2).map(|&n| Color::Indexed(n as u8)),
            2 => {
                // With a colour-space id: 38:2:<id>:r:g:b; without: 38:2:r:g:b.
                let rgb = if p.len() >= 6 { &p[3..6] } else { p.get(2..5)? };
                Some(Color::Rgb(Rgb::new(rgb[0] as u8, rgb[1] as u8, rgb[2] as u8)))
            }
            _ => None,
        };
    }
    let next = |offset: usize| params.get(*i + offset).map(|p| p[0]);
    match next(1)? {
        5 => {
            let n = next(2)?;
            *i += 2;
            Some(Color::Indexed(n as u8))
        }
        2 => {
            let (r, g, b) = (next(2)?, next(3)?, next(4)?);
            *i += 4;
            Some(Color::Rgb(Rgb::new(r as u8, g as u8, b as u8)))
        }
        _ => None,
    }
}

/// The SGR string recreating `template`, for DECRQSS.
fn sgr_string(template: &Cell) -> String {
    let mut parts = vec!["0".to_string()];
    let flags = [
        (Flags::BOLD, "1"),
        (Flags::DIM, "2"),
        (Flags::ITALIC, "3"),
        (Flags::UNDERLINE, "4"),
        (Flags::BLINK, "5"),
        (Flags::INVERSE, "7"),
        (Flags::HIDDEN, "8"),
        (Flags::STRIKE, "9"),
    ];
    for (flag, code) in flags {
        if template.flags.contains(flag) {
            parts.push(code.to_string());
        }
    }
    for (color, base) in [(template.fg, 38), (template.bg, 48)] {
        match color {
            Color::Default => {}
            Color::Indexed(n) => parts.push(format!("{base}:5:{n}")),
            Color::Rgb(c) => parts.push(format!("{base}:2::{}:{}:{}", c.r, c.g, c.b)),
        }
    }
    format!("{}m", parts.join(";"))
}

/// Answers for XTGETTCAP. `Some(None)` is a boolean capability that is present.
fn termcap_value(name: &str) -> Option<Option<&'static str>> {
    Some(match name {
        "TN" | "name" => Some("xterm-ionterm"),
        "Co" | "colors" => Some("256"),
        "RGB" => Some("8/8/8"),
        "Tc" | "Su" | "bce" | "am" => None,
        "Smulx" => Some("\x1b[4:%p1%dm"),
        "Setulc" => Some("\x1b[58:2::%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m"),
        "Ss" => Some("\x1b[%p1%d q"),
        "Se" => Some("\x1b[2 q"),
        "Ms" => Some("\x1b]52;%p1%s;%p2%s\x07"),
        "Sync" => Some("\x1b[?2026%?%p1%{1}%-%tl%eh%;"),
        "BE" => Some("\x1b[?2004h"),
        "BD" => Some("\x1b[?2004l"),
        "PS" => Some("\x1b[200~"),
        "PE" => Some("\x1b[201~"),
        "fe" => Some("\x1b[?1004h"),
        "fd" => Some("\x1b[?1004l"),
        _ => return None,
    })
}

fn hex_decode(hex: &[u8]) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    hex.chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(cols: usize, rows: usize) -> (Term, Processor) {
        (Term::new(cols, rows, 100, Palette::default()), Processor::new())
    }

    fn line(term: &Term, row: usize) -> String {
        term.grid[row].cells.iter().map(|c| c.c).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn prints_and_wraps() {
        let (mut t, mut p) = term(5, 3);
        p.advance(&mut t, b"helloworld!");
        assert_eq!(line(&t, 0), "hello");
        assert_eq!(line(&t, 1), "world");
        assert_eq!(line(&t, 2), "!");
        assert!(t.grid[0].wrapped);
    }

    #[test]
    fn scrolls_into_scrollback() {
        let (mut t, mut p) = term(10, 2);
        p.advance(&mut t, b"one\r\ntwo\r\nthree");
        assert_eq!(t.scrollback.len(), 1);
        assert_eq!(line(&t, 0), "two");
        assert_eq!(line(&t, 1), "three");
        t.scroll_display(1);
        let snap = t.snapshot();
        assert_eq!(snap.lines[0].cells[0].c, 'o');
        assert!(snap.cursor.is_none());
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let (mut t, mut p) = term(4, 2);
        p.advance(&mut t, "a字".as_bytes());
        assert!(t.grid[0].cells[1].flags.contains(Flags::WIDE));
        assert!(t.grid[0].cells[2].flags.contains(Flags::WIDE_SPACER));
        // Overwriting the spacer clears the orphaned first half.
        p.advance(&mut t, b"\x1b[1;3Hx");
        assert_eq!(t.grid[0].cells[1].c, ' ');
    }

    #[test]
    fn sgr_truecolor_both_forms() {
        let (mut t, mut p) = term(10, 2);
        p.advance(&mut t, b"\x1b[38;2;1;2;3;48:2::4:5:6mx");
        let cell = &t.grid[0].cells[0];
        assert_eq!(cell.fg, Color::Rgb(Rgb::new(1, 2, 3)));
        assert_eq!(cell.bg, Color::Rgb(Rgb::new(4, 5, 6)));
    }

    #[test]
    fn alt_screen_round_trip() {
        let (mut t, mut p) = term(10, 2);
        p.advance(&mut t, b"main\x1b[?1049hALT");
        // The cursor keeps its column when switching screens.
        assert_eq!(line(&t, 0), "    ALT");
        p.advance(&mut t, b"\x1b[?1049l");
        assert_eq!(line(&t, 0), "main");
        assert_eq!(t.cursor.col, 4);
    }

    #[test]
    fn answers_queries() {
        let (mut t, mut p) = term(10, 5);
        p.advance(&mut t, b"\x1b[3;4H\x1b[6n\x1b[?u\x1b[?2026$p");
        assert_eq!(t.take_output(), b"\x1b[3;4R\x1b[?0u\x1b[?2026;2$y");
        p.advance(&mut t, b"\x1b[>1u\x1b[?u");
        assert_eq!(t.take_output(), b"\x1b[?1u");
        // XTGETTCAP for "Co".
        p.advance(&mut t, b"\x1bP+q436f\x1b\\");
        assert_eq!(t.take_output(), b"\x1bP1+r436f=323536\x1b\\");
    }

    #[test]
    fn scroll_region() {
        let (mut t, mut p) = term(5, 4);
        p.advance(&mut t, b"a\r\nb\r\nc\r\nd\x1b[2;3r\x1b[3;1H\n");
        assert_eq!(line(&t, 0), "a");
        assert_eq!(line(&t, 1), "c");
        assert_eq!(line(&t, 2), "");
        assert_eq!(line(&t, 3), "d");
        // Region scrolls never reach the scrollback.
        assert!(t.scrollback.is_empty());
    }

    #[test]
    fn resize_keeps_cursor_line() {
        let (mut t, mut p) = term(10, 4);
        p.advance(&mut t, b"1\r\n2\r\n3\r\n4");
        t.resize(10, 2);
        assert_eq!(line(&t, 0), "3");
        assert_eq!(line(&t, 1), "4");
        assert_eq!(t.cursor.row, 1);
        t.resize(10, 4);
        assert_eq!(line(&t, 0), "1");
        assert_eq!(t.cursor.row, 3);
    }
}
