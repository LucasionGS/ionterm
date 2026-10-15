//! The terminal state machine: everything a `vte::Perform` callback can change.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use bitflags::bitflags;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

use crate::base64;
use crate::cell::{Cell, Flags, Row};
use crate::charset::Charset;
use crate::color::{Color, Palette, Rgb};
use crate::graphics::{Anchor, Command, Graphics, GraphicsError, Image, ImageDraw, Placement};
use crate::sixel::SixelDecoder;
use crate::reflow::{Reflowed, reflow, wide_padding};

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Largest DCS payload we buffer. Sixel images will need their own streaming
/// path; this only guards the small query sequences.
const MAX_DCS_LEN: usize = 64 * 1024;
/// Longest APC sequence buffered (a graphics command's chunk). Clients send
/// at most 4 KiB per chunk; anything far larger is a broken or hostile stream.
const MAX_APC_LEN: usize = 16 * 1024 * 1024;
/// Distinct hyperlinks remembered per terminal; later ones aren't linked.
const MAX_HYPERLINKS: usize = 65_536;
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
        /// DECSDM: sixel images go to the top-left corner and don't scroll.
        const SIXEL_DISPLAY = 1 << 21;

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

/// Progress a program reports for itself (OSC 9;4, as in Windows Terminal).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    pub state: ProgressState,
    /// 0 to 100; `None` while indeterminate.
    pub percent: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressState {
    Normal,
    Error,
    Paused,
    Indeterminate,
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
    /// OSC 5379, ionterm's own: the remote shell's `ionterm-get` or
    /// `ionterm-put` asks for a download or an upload.
    FileRequest(FileRequest),
    /// OSC 9;4 set or cleared the reported progress.
    Progress(Option<Progress>),
    /// The shell shows its prompt again (OSC 133;A): any command is over.
    Prompt,
    /// A command started (OSC 133;C), with its command line when the shell
    /// sends it (`cmdline_url=` from fish, `cmdline=` from zsh).
    CommandStarted(Option<String>),
    /// The running command finished (OSC 133;D), with its exit status.
    CommandFinished(Option<i32>),
    /// A program asked for a desktop notification (OSC 9 or OSC 777).
    Notify { title: String, body: String },
}

/// What `ionterm-get` and `ionterm-put` ask for, with absolute remote paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileRequest {
    /// Download this file (`dir` = it's a folder).
    Get { path: String, dir: bool },
    /// Choose local files and upload them into this folder.
    Put { dir: String },
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
    Sixel(Box<SixelDecoder>),
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
    /// Anchored image placements reaching into the viewport.
    pub images: Vec<ImageDraw>,
    /// Placements shown through unicode placeholder cells.
    pub virtual_images: Vec<(Image, Placement)>,
}

/// Feeds PTY output into a [`Term`], keeping parser state between reads so an
/// escape sequence split across two reads still parses.
///
/// `vte` discards APC strings, which carry kitty graphics, so they are cut
/// out here before the rest of the stream reaches it.
#[derive(Default)]
pub struct Processor {
    parser: vte::Parser,
    apc: ApcState,
}

#[derive(Default)]
enum ApcState {
    #[default]
    Ground,
    /// Saw ESC; the next byte decides whether an APC starts.
    Escape,
    Apc(Vec<u8>),
    /// Saw ESC inside an APC; `\` ends it.
    ApcEscape(Vec<u8>),
}

impl Processor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&mut self, term: &mut Term, bytes: &[u8]) {
        let mut i = 0;
        // Start of bytes not yet handed to vte.
        let mut pending = 0;
        while i < bytes.len() {
            match std::mem::take(&mut self.apc) {
                ApcState::Ground => match bytes[i..].iter().position(|&b| b == 0x1b) {
                    Some(at) => {
                        self.parser.advance(term, &bytes[pending..i + at]);
                        i += at + 1;
                        pending = i;
                        self.apc = ApcState::Escape;
                    }
                    None => {
                        i = bytes.len();
                    }
                },
                ApcState::Escape => {
                    if bytes[i] == b'_' {
                        i += 1;
                        pending = i;
                        self.apc = ApcState::Apc(Vec::new());
                    } else {
                        // Not an APC: give vte the ESC and carry on normally.
                        self.parser.advance(term, &[0x1b]);
                        pending = i;
                    }
                }
                ApcState::Apc(mut buf) => {
                    let end = bytes[i..].iter().position(|&b| b == 0x1b).map(|at| i + at);
                    let chunk = &bytes[i..end.unwrap_or(bytes.len())];
                    if buf.len() + chunk.len() <= MAX_APC_LEN {
                        buf.extend_from_slice(chunk);
                    }
                    match end {
                        Some(at) => {
                            i = at + 1;
                            self.apc = ApcState::ApcEscape(buf);
                        }
                        None => {
                            i = bytes.len();
                            self.apc = ApcState::Apc(buf);
                        }
                    }
                    pending = i;
                }
                ApcState::ApcEscape(buf) => {
                    if bytes[i] == b'\\' {
                        i += 1;
                        if buf.len() < MAX_APC_LEN {
                            term.apc_dispatch(&buf);
                        }
                    } else {
                        // An ESC that doesn't end the string aborts it; the
                        // byte after it starts a new escape sequence.
                        self.parser.advance(term, &[0x1b]);
                    }
                    pending = i;
                }
            }
        }
        if matches!(self.apc, ApcState::Ground) && pending < bytes.len() {
            self.parser.advance(term, &bytes[pending..]);
        }
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
    /// Between a prompt mark (OSC 133;A) and the command starting (133;C):
    /// the shell is showing its prompt and will redraw it after a resize.
    at_prompt: bool,
    /// Between a prompt mark and the command starting, whether or not the
    /// shell redraws its prompt.
    editing: bool,
    /// The shell takes mouse clicks on its command line (OSC 133;A with
    /// `click_events=1`): they move its cursor.
    click_events: bool,
    /// A command started (OSC 133;C) and hasn't finished (133;D).
    command_running: bool,
    /// The row a carriage return went back to, while it's rewritten in
    /// place (how curl or pacman draw progress); a line feed ends that.
    rewritten_row: Option<usize>,
    /// From OSC 9;4.
    progress: Option<Progress>,
    graphics: Graphics,
    /// Whether the device attributes reply announces sixel support.
    advertise_sixel: bool,
    /// OSC 8 targets; a cell's `hyperlink` is an index into this plus one.
    hyperlinks: Vec<String>,
    /// (OSC 8 `id` parameter, URI) → hyperlink id, so repeated links share one.
    hyperlink_ids: HashMap<(String, String), u32>,
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
            at_prompt: false,
            editing: false,
            click_events: false,
            command_running: false,
            rewritten_row: None,
            progress: None,
            graphics: Graphics::default(),
            advertise_sixel: false,
            hyperlinks: Vec::new(),
            hyperlink_ids: HashMap::new(),
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

    /// The URI of a cell's OSC 8 hyperlink.
    pub fn hyperlink(&self, id: u32) -> Option<&str> {
        let index = (id as usize).checked_sub(1)?;
        self.hyperlinks.get(index).map(String::as_str)
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

    /// Puts back the modes a crashed or disconnected program may have left
    /// on: normal screen, visible cursor, no mouse or keyboard protocols,
    /// plain colours.
    pub fn reset_modes(&mut self) {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.leave_alt_screen(true, false);
        }
        self.mode = TermMode::AUTOWRAP | TermMode::SHOW_CURSOR;
        self.keyboard_stacks = [Vec::new(), Vec::new()];
        self.cursor.template = Cell::default();
        self.cursor_style = None;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.dirty = true;
    }

    /// Sixel images are always decoded; this only decides whether DA1 says
    /// so. Some programs (yazi) pick sixel over the kitty protocol when both
    /// are announced, which looks worse, so it is off by default.
    pub fn set_advertise_sixel(&mut self, advertise: bool) {
        self.advertise_sixel = advertise;
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

    //#region Shell integration

    /// Scrolls so an absolute line is at the top (or as near as it gets).
    pub fn scroll_to_line(&mut self, absolute: usize) {
        let offset = self.scrollback.len().saturating_sub(absolute);
        self.scroll_display(offset as isize - self.display_offset as isize);
    }

    /// Absolute lines where a shell prompt starts (OSC 133;A), oldest first.
    pub fn prompt_lines(&self) -> Vec<usize> {
        (0..self.total_lines()).filter(|&i| self.line(i).is_some_and(|r| r.prompt)).collect()
    }

    fn total_lines(&self) -> usize {
        self.scrollback.len() + self.grid.len()
    }

    /// The output of the command around an absolute line, as the first and
    /// last line of it: from where the command started writing (133;C) to
    /// the line before the next prompt. `None` without marks or output.
    pub fn command_output_at(&self, absolute: usize) -> Option<(usize, usize)> {
        let marked = |i: usize, f: fn(&Row) -> bool| self.line(i).is_some_and(f);
        // The prompt this line belongs to, and the next one.
        let prompt = (0..=absolute.min(self.total_lines().checked_sub(1)?)).rev().find(|&i| marked(i, |r| r.prompt))?;
        let next = (prompt + 1..self.total_lines()).find(|&i| marked(i, |r| r.prompt));
        let limit = next.unwrap_or(self.total_lines());
        let start = (prompt + 1..limit).find(|&i| marked(i, |r| r.output))?;
        // A running command's output ends at the cursor.
        let mut end = match next {
            Some(next) => next - 1,
            None => self.scrollback.len() + self.cursor.row,
        };
        let blank = |i: usize| self.line(i).is_none_or(|r| r.cells.iter().all(|c| c.c == ' ' && c.extra.is_none()));
        while end > start && blank(end) {
            end -= 1;
        }
        (end >= start && !(end == start && blank(start))).then_some((start, end))
    }

    /// The output of the last command that wrote any, finished or running.
    pub fn last_command_output(&self) -> Option<(usize, usize)> {
        self.prompt_lines().into_iter().rev().find_map(|p| self.command_output_at(p))
    }

    /// Viewport rows whose prompt's command failed, with its exit status.
    pub fn failed_commands_in_view(&self) -> Vec<(usize, i32)> {
        (0..self.rows)
            .filter_map(|row| {
                let status = self.line(self.viewport_to_absolute(row))?.status?;
                (status != 0).then_some((row, status))
            })
            .collect()
    }

    /// Whether a left click at a viewport row goes to the shell (to move
    /// its cursor) rather than starting a selection: the shell asked for
    /// clicks, and it's on the command line being edited.
    pub fn click_goes_to_shell(&self, row: usize) -> bool {
        if !self.click_events || !self.editing || self.display_offset != 0 || self.mode.contains(TermMode::ALT_SCREEN) {
            return false;
        }
        let Some(prompt) = (0..=self.cursor.row).rev().find(|&r| self.grid[r].prompt) else { return false };
        // The command line may go on below the cursor.
        let mut end = self.cursor.row;
        while end + 1 < self.grid.len() && self.grid[end].wrapped {
            end += 1;
        }
        (prompt..=end).contains(&row)
    }

    /// The progress a program reported (OSC 9;4).
    pub fn progress(&self) -> Option<Progress> {
        self.progress
    }

    /// A percentage on the line a running command keeps rewriting in place
    /// (how curl, wget and pacman draw progress), as 0 to 100. Prompts are
    /// left out: they're redrawn the same way and may show a battery level.
    pub fn progress_hint(&self) -> Option<f32> {
        if self.editing || self.at_prompt || self.mode.contains(TermMode::ALT_SCREEN) {
            return None;
        }
        let row = self.rewritten_row.filter(|&r| r == self.cursor.row)?;
        // What's after the cursor is left over from a longer line before.
        let written = &self.grid[row].cells[..self.cursor.col.min(self.cols)];
        last_percentage(&written.iter().map(|c| c.c).collect::<String>())
    }

    /// Records an exit status on the prompt the finished command came from.
    fn mark_status(&mut self, status: i32) {
        if let Some(row) = (0..=self.cursor.row).rev().find(|&r| self.grid[r].prompt) {
            self.grid[row].status = Some(status);
        } else if let Some(row) = self.scrollback.iter_mut().rev().find(|r| r.prompt) {
            row.status = Some(status);
        }
        self.dirty = true;
    }

    //#endregion

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        let alt_active = self.mode.contains(TermMode::ALT_SCREEN);

        let mut cursor_shift: isize = 0;
        if cols != self.cols {
            self.reflow_primary(cols, rows);
            self.resize_alternate(cols, rows);
        } else {
            // Rows only. On the primary screen, lines leaving the top go to the
            // scrollback, and growing pulls them back, like most terminals do.
            let (primary, alternate) = if alt_active {
                (&mut self.other_grid, &mut self.grid)
            } else {
                (&mut self.grid, &mut self.other_grid)
            };
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

    /// Re-wraps the primary screen and its scrollback to a new width.
    ///
    /// While the shell sits at its prompt, the prompt's rows are blanked
    /// instead: the shell redraws its prompt after a resize, moving up by the
    /// number of rows it drew. Reflowing them would change that count and
    /// leave a stray copy of the prompt above the redrawn one.
    fn reflow_primary(&mut self, cols: usize, rows: usize) {
        let alt_active = self.mode.contains(TermMode::ALT_SCREEN);
        let prompt_start = (self.at_prompt && !alt_active)
            .then(|| (0..=self.cursor.row).rev().find(|&row| self.grid[row].prompt))
            .flatten();
        // While the alternate screen is up, the primary cursor is the one saved on entry.
        let cursor = if alt_active {
            self.saved_primary
                .as_ref()
                .map_or((self.rows - 1, 0), |saved| (saved.cursor.row, saved.cursor.col))
        } else {
            (self.cursor.row, self.cursor.col)
        };
        let mut grid = std::mem::take(if alt_active { &mut self.other_grid } else { &mut self.grid });
        // Rows from the prompt to the cursor, kept out of the reflow.
        let prompt_rows = prompt_start.map(|start| {
            grid.truncate(cursor.0 + 1);
            grid.split_off(start).len()
        });
        let history = self.scrollback.len();
        let all: Vec<Row> = self.scrollback.drain(..).chain(grid).collect();
        let Reflowed { rows: mut lines, cursor } = match prompt_rows {
            Some(count) => {
                let mut out = if all.is_empty() {
                    Vec::new()
                } else {
                    // Content above the prompt; its cursor is irrelevant.
                    let last = all.len() - 1;
                    reflow(all, (last, 0), cols).rows
                };
                let first = out.len();
                out.extend((0..count).map(|_| Row::new(cols, Color::Default)));
                out[first].prompt = true;
                Reflowed {
                    rows: out,
                    cursor: (first + count - 1, cursor.1.min(cols - 1)),
                }
            }
            None => reflow(all, (history + cursor.0, cursor.1), cols),
        };

        // The screen is the last `rows` lines, moved up if needed to keep the cursor on it.
        let start = lines.len().saturating_sub(rows).min(cursor.0);
        let mut screen = lines.split_off(start);
        screen.truncate(rows);
        screen.resize(rows, Row::new(cols, Color::Default));
        self.scrollback = lines.into();
        let (row, col) = (cursor.0 - start, cursor.1);

        if alt_active {
            self.other_grid = screen;
            if let Some(saved) = &mut self.saved_primary {
                saved.cursor.row = row;
                saved.cursor.col = col;
            }
        } else {
            self.grid = screen;
            self.cursor.row = row;
            self.cursor.col = col;
        }
    }

    /// The alternate screen isn't reflowed: full-screen apps redraw on resize.
    fn resize_alternate(&mut self, cols: usize, rows: usize) {
        let alt_active = self.mode.contains(TermMode::ALT_SCREEN);
        let grid = if alt_active { &mut self.grid } else { &mut self.other_grid };
        for row in grid.iter_mut() {
            row.resize(cols);
        }
        if rows < grid.len() {
            let overflow = if alt_active { (self.cursor.row + 1).saturating_sub(rows) } else { 0 };
            grid.drain(..overflow);
            grid.truncate(rows);
            if alt_active {
                self.cursor.row -= overflow;
            }
        }
        grid.resize(rows, Row::new(cols, Color::Default));
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

        let (images, virtual_images) = self.visible_images(offset);
        Snapshot {
            cols: self.cols,
            rows: self.rows,
            lines,
            cursor,
            palette: self.palette.clone(),
            display_offset: offset,
            scrollback_len: self.scrollback.len(),
            images,
            virtual_images,
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
            if let Some(row) = self.scrollback.pop_front() {
                self.forget_anchors(&row);
            }
        }
    }

    /// Removes the placements anchored on a row that is going away.
    fn forget_anchors(&mut self, row: &Row) {
        if row.anchors.is_empty() {
            return;
        }
        let keys: Vec<u64> = row.anchors.iter().map(|a| a.placement).collect();
        self.graphics.remove_placements(false, |key, _| keys.contains(&key));
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
        if flags.contains(Flags::WIDE_SPACER) && col > 0 && cells[col - 1].flags.contains(Flags::WIDE) {
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
            let blank = self.blank_row();
            let old = std::mem::replace(&mut self.grid[row], blank);
            self.forget_anchors(&old);
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
            80 => TermMode::SIXEL_DISPLAY,
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
            80 => TermMode::SIXEL_DISPLAY,
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
        // SGR 0 resets styling, but a hyperlink only ends with an empty OSC 8.
        let hyperlink = self.cursor.template.hyperlink;
        if params.is_empty() {
            self.cursor.template = Cell {
                hyperlink,
                ..Cell::default()
            };
            return;
        }
        let t = &mut self.cursor.template;
        let mut i = 0;
        while i < params.len() {
            let p = params[i];
            match p[0] {
                0 => {
                    *t = Cell {
                        hyperlink,
                        ..Cell::default()
                    }
                }
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
            Dcs::Sixel(decoder) => {
                if let Some((width, height, rgba)) = decoder.finish() {
                    self.place_sixel(width, height, rgba);
                }
            }
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

//#region Graphics

impl Term {
    /// An APC string from the [`Processor`]; kitty graphics start with `G`.
    fn apc_dispatch(&mut self, data: &[u8]) {
        let Some(rest) = data.strip_prefix(b"G") else { return };
        let (control, payload) = match rest.iter().position(|&b| b == b';') {
            Some(at) => (&rest[..at], &rest[at + 1..]),
            None => (rest, &[][..]),
        };
        let cmd = Command::parse(control);
        if let Some(finished) = self.graphics.continue_loading(&cmd, payload) {
            if let Some((first, data)) = finished {
                self.graphics_execute(first, data);
            }
            return;
        }
        if cmd.more && matches!(cmd.action, b't' | b'T' | b'q') {
            self.graphics.start_loading(cmd, payload);
            return;
        }
        let data = base64::decode(payload).unwrap_or_default();
        self.graphics_execute(cmd, data);
    }

    fn graphics_execute(&mut self, cmd: Command, data: Vec<u8>) {
        let result = match cmd.action {
            // Queries check that a transmission would work, without storing it.
            b'q' => Graphics::default().transmit(&cmd, data).map(|_| cmd.id),
            b't' => self.graphics.transmit(&cmd, data),
            b'T' => self.graphics.transmit(&cmd, data).inspect(|&id| self.place_image(id, &cmd)),
            b'p' => match self.graphics.resolve(&cmd) {
                Some(id) => {
                    self.place_image(id, &cmd);
                    Ok(id)
                }
                None => Err(GraphicsError {
                    code: "ENOENT",
                    message: "no such image".into(),
                }),
            },
            b'd' => {
                self.delete_images(&cmd);
                return;
            }
            other => Err(GraphicsError {
                code: "EINVAL",
                message: format!("unsupported action {:?}", other as char),
            }),
        };
        // Replies go only to clients that named the image, and not when quiet.
        if cmd.id == 0 && cmd.number == 0 {
            return;
        }
        let (id, message) = match &result {
            Ok(id) if cmd.quiet == 0 => (*id, "OK".to_string()),
            Err(error) if cmd.quiet < 2 => (cmd.id, format!("{}:{}", error.code, error.message)),
            _ => return,
        };
        let mut keys = format!("i={id}");
        if cmd.number != 0 {
            keys.push_str(&format!(",I={}", cmd.number));
        }
        if cmd.placement != 0 {
            keys.push_str(&format!(",p={}", cmd.placement));
        }
        self.reply(&format!("\x1b_G{keys};{message}\x1b\\"));
    }

    fn place_image(&mut self, image_id: u32, cmd: &Command) {
        let key = self.graphics.place(image_id, cmd, (self.cell_width, self.cell_height));
        self.dirty = true;
        if cmd.unicode {
            return;
        }
        let (row, col) = (self.cursor.row, self.cursor.col);
        self.grid[row].anchors.push(Anchor { placement: key, col });
        if cmd.cursor_movement == 1 {
            return;
        }
        // The cursor ends up just right of the image, on its last row.
        let Some(p) = self.graphics.placement(key) else { return };
        let (rows, cols) = (p.rows as usize, p.cols as usize);
        for _ in 1..rows {
            self.linefeed();
        }
        let next = col + cols;
        if next >= self.cols {
            self.cursor.col = self.cols - 1;
            self.cursor.wrap_pending = self.mode.contains(TermMode::AUTOWRAP);
        } else {
            self.cursor.col = next;
            self.cursor.wrap_pending = false;
        }
    }

    /// Shows a decoded sixel image at the cursor. Unless sixel display mode is
    /// on, the cursor then moves to the line below it, like xterm's.
    fn place_sixel(&mut self, width: u32, height: u32, rgba: Vec<u8>) {
        let id = self.graphics.store(0, 0, width, height, rgba);
        let key = self.graphics.place(id, &Command::default(), (self.cell_width, self.cell_height));
        self.dirty = true;
        if self.mode.contains(TermMode::SIXEL_DISPLAY) {
            self.grid[0].anchors.push(Anchor { placement: key, col: 0 });
            return;
        }
        let (row, col) = (self.cursor.row, self.cursor.col);
        self.grid[row].anchors.push(Anchor { placement: key, col });
        let rows = self.graphics.placement(key).map_or(1, |p| p.rows as usize);
        for _ in 0..rows {
            self.linefeed();
        }
        self.cursor.col = col;
    }

    /// Anchored placements on the current screen: (key, row, col, placement).
    fn screen_placements(&self) -> Vec<(u64, usize, usize, Placement)> {
        let mut out = Vec::new();
        for (row, line) in self.grid.iter().enumerate() {
            for anchor in &line.anchors {
                if let Some(p) = self.graphics.placement(anchor.placement) {
                    out.push((anchor.placement, row, anchor.col, p.clone()));
                }
            }
        }
        out
    }

    fn delete_images(&mut self, cmd: &Command) {
        let free = cmd.delete.is_ascii_uppercase();
        let covers = |row: usize, col: usize, p: &Placement, x: Option<usize>, y: Option<usize>| {
            x.is_none_or(|x| x >= col && x < col + p.cols as usize) && y.is_none_or(|y| y >= row && y < row + p.rows as usize)
        };
        let on_screen = |filter: &dyn Fn(usize, usize, &Placement) -> bool| -> Vec<u64> {
            self.screen_placements()
                .into_iter()
                .filter(|(_, row, col, p)| filter(*row, *col, p))
                .map(|(key, ..)| key)
                .collect()
        };
        let (cx, cy) = (cmd.x.saturating_sub(1) as usize, cmd.y.saturating_sub(1) as usize);
        let keys: Vec<u64> = match cmd.delete.to_ascii_lowercase() {
            b'a' => on_screen(&|_, _, _| true),
            b'c' => {
                let (cursor_x, cursor_y) = (self.cursor.col, self.cursor.row);
                on_screen(&|row, col, p| covers(row, col, p, Some(cursor_x), Some(cursor_y)))
            }
            b'p' => on_screen(&|row, col, p| covers(row, col, p, Some(cx), Some(cy))),
            b'q' => on_screen(&|row, col, p| covers(row, col, p, Some(cx), Some(cy)) && p.z == cmd.z),
            b'x' => on_screen(&|row, col, p| covers(row, col, p, Some(cx), None)),
            b'y' => on_screen(&|row, col, p| covers(row, col, p, None, Some(cy))),
            b'z' => on_screen(&|_, _, p| p.z == cmd.z),
            b'i' | b'n' => {
                let id = if cmd.delete.eq_ignore_ascii_case(&b'n') {
                    let Some(id) = self.graphics.resolve(&Command { id: 0, ..cmd.clone() }) else { return };
                    id
                } else {
                    cmd.id
                };
                let placement = cmd.placement;
                self.graphics.remove_placements(free, |_, p| {
                    p.image_id == id && (placement == 0 || p.placement_id == placement)
                });
                if free && placement == 0 {
                    self.graphics.remove_image(id);
                }
                self.dirty = true;
                return;
            }
            b'r' => {
                let range = cmd.x..=cmd.y;
                self.graphics.remove_placements(free, |_, p| range.contains(&p.image_id));
                self.dirty = true;
                return;
            }
            _ => return,
        };
        self.graphics.remove_placements(free, |key, _| keys.contains(&key));
        self.dirty = true;
    }

    /// Placements reaching into the viewport, and virtual placements.
    fn visible_images(&self, offset: usize) -> (Vec<ImageDraw>, Vec<(Image, Placement)>) {
        if self.graphics.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let top = self.scrollback.len() - offset;
        let mut images = Vec::new();
        for line in top.saturating_sub(self.graphics.max_rows)..top + self.rows {
            let Some(row) = self.line(line) else { break };
            for anchor in &row.anchors {
                let Some(p) = self.graphics.placement(anchor.placement) else { continue };
                let Some(image) = self.graphics.image(p.image_id) else { continue };
                let viewport_row = line as isize - top as isize;
                if viewport_row + p.rows as isize > 0 {
                    images.push(ImageDraw {
                        image: image.clone(),
                        placement: p.clone(),
                        row: viewport_row,
                        col: anchor.col,
                    });
                }
            }
        }
        images.sort_by_key(|draw| draw.placement.z);
        let virtual_images = self
            .graphics
            .virtual_placements()
            .filter_map(|p| Some((self.graphics.image(p.image_id)?.clone(), p.clone())))
            .collect();
        (images, virtual_images)
    }
}

//#endregion

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
                // Mark the unused last column so reflow doesn't read it as a space.
                let last = self.cols - 1;
                self.fix_wide_fragments(self.cursor.row, last);
                self.grid[self.cursor.row].cells[last] = wide_padding();
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
                self.rewritten_row = None;
                self.linefeed();
                if self.mode.contains(TermMode::LINEFEED_NEWLINE) {
                    self.cursor.col = 0;
                }
            }
            0x0d => {
                self.cursor.col = 0;
                self.cursor.wrap_pending = false;
                self.rewritten_row = Some(self.cursor.row);
            }
            0x0e => self.active_charset = 1,
            0x0f => self.active_charset = 0,
            _ => tracing::debug!("unhandled control {byte:#04x}"),
        }
    }

    fn hook(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        self.dcs = match (intermediates, action) {
            ([b'+'], 'q') => Some(Dcs::GetTermcap(Vec::new())),
            ([b'$'], 'q') => Some(Dcs::RequestSetting(Vec::new())),
            ([], 'q') => {
                let params: Vec<u16> = params.iter().map(|p| p[0]).collect();
                let bg = self.palette.background;
                Some(Dcs::Sixel(Box::new(SixelDecoder::new(&params, [bg.r, bg.g, bg.b]))))
            }
            _ => None,
        };
    }

    fn put(&mut self, byte: u8) {
        match &mut self.dcs {
            Some(Dcs::Sixel(decoder)) => decoder.put(byte),
            Some(Dcs::GetTermcap(data) | Dcs::RequestSetting(data)) if data.len() < MAX_DCS_LEN => data.push(byte),
            _ => {}
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
            "8" => {
                // OSC 8 ; params ; URI. The URI may itself contain `;`.
                let id_param = params
                    .get(1)
                    .map(|p| String::from_utf8_lossy(p))
                    .and_then(|p| p.split(':').find_map(|kv| kv.strip_prefix("id=").map(str::to_string)))
                    .unwrap_or_default();
                let uri = params
                    .get(2..)
                    .unwrap_or_default()
                    .iter()
                    .map(|p| String::from_utf8_lossy(p))
                    .collect::<Vec<_>>()
                    .join(";");
                self.cursor.template.hyperlink = if uri.is_empty() {
                    0
                } else if let Some(&id) = self.hyperlink_ids.get(&(id_param.clone(), uri.clone())) {
                    id
                } else if self.hyperlinks.len() < MAX_HYPERLINKS {
                    self.hyperlinks.push(uri.clone());
                    let id = self.hyperlinks.len() as u32;
                    self.hyperlink_ids.insert((id_param, uri), id);
                    id
                } else {
                    0
                };
            }
            "133" => match params.get(1).map(|p| p.first().copied()) {
                // Prompt start. `redraw=0` means the shell won't redraw it on
                // resize; `click_events=1` that it takes clicks on its line.
                Some(Some(b'A')) => {
                    // A secondary prompt (`k=s`, PS2) continues the command line.
                    if params[2..].iter().any(|p| *p == b"k=s") {
                        return;
                    }
                    let redraws = !params[2..].iter().any(|p| *p == b"redraw=0");
                    self.at_prompt = redraws;
                    self.editing = true;
                    self.click_events = params[2..].iter().any(|p| *p == b"click_events=1");
                    self.command_running = false;
                    let row = self.cursor.row;
                    self.grid[row].prompt = true;
                    self.events.push(TermEvent::Prompt);
                }
                // Command started: its output begins here.
                Some(Some(b'C')) => {
                    self.at_prompt = false;
                    self.editing = false;
                    self.command_running = true;
                    let row = self.cursor.row;
                    self.grid[row].output = true;
                    self.events.push(TermEvent::CommandStarted(command_line(&params[2..])));
                }
                // Command finished, with its exit status.
                Some(Some(b'D')) => {
                    self.at_prompt = false;
                    self.editing = false;
                    let status = params
                        .get(2)
                        .and_then(|p| std::str::from_utf8(p).ok())
                        .and_then(|p| p.parse::<i32>().ok());
                    if std::mem::take(&mut self.command_running) {
                        if let Some(status) = status {
                            self.mark_status(status);
                        }
                        self.events.push(TermEvent::CommandFinished(status));
                    }
                }
                _ => {}
            },
            // OSC 9 ; text: a notification (iTerm2, ConEmu).
            "9" if params.get(1).is_some_and(|p| *p != b"4") => {
                let body = params[1..].iter().map(|p| String::from_utf8_lossy(p)).collect::<Vec<_>>().join(";");
                self.events.push(TermEvent::Notify { title: String::new(), body });
            }
            // OSC 777 ; notify ; title ; body (urxvt, foot, Ghostty).
            "777" if params.get(1).copied() == Some(b"notify") => {
                let text = |i: usize| params.get(i).map(|p| String::from_utf8_lossy(p).into_owned()).unwrap_or_default();
                let body = params.get(3..).map(|rest| rest.iter().map(|p| String::from_utf8_lossy(p)).collect::<Vec<_>>().join(";")).unwrap_or_default();
                self.events.push(TermEvent::Notify { title: text(2), body });
            }
            // OSC 9 ; 4 ; state ; percent (ConEmu, Windows Terminal).
            "9" if params.get(1).copied() == Some(b"4") => {
                let number = |i: usize| {
                    params
                        .get(i)
                        .and_then(|p| std::str::from_utf8(p).ok())
                        .and_then(|p| p.parse::<u32>().ok())
                };
                let percent = number(3).map(|p| p.min(100) as u8);
                let progress = match number(2) {
                    Some(1) => Some(ProgressState::Normal),
                    Some(2) => Some(ProgressState::Error),
                    Some(3) => Some(ProgressState::Indeterminate),
                    Some(4) => Some(ProgressState::Paused),
                    _ => None,
                }
                .map(|state| Progress {
                    state,
                    percent: if state == ProgressState::Indeterminate { None } else { Some(percent.unwrap_or(0)) },
                });
                if progress != self.progress {
                    self.progress = progress;
                    self.events.push(TermEvent::Progress(progress));
                }
            }
            "10" | "11" | "12" => {
                let first = code.parse().expect("matched digits");
                self.osc_dynamic_colors(first, &params[1..], bell_terminated);
            }
            "5379" => {
                // OSC 5379 ; get | get-dir | put ; <base64 path>
                let path = params.get(2).and_then(|p| base64::decode(p)).map(|b| String::from_utf8_lossy(&b).into_owned());
                let request = match (params.get(1).copied(), path) {
                    (Some(b"get"), Some(path)) => Some(FileRequest::Get { path, dir: false }),
                    (Some(b"get-dir"), Some(path)) => Some(FileRequest::Get { path, dir: true }),
                    (Some(b"put"), Some(dir)) => Some(FileRequest::Put { dir }),
                    _ => None,
                };
                if let Some(request) = request.filter(|r| match r {
                    FileRequest::Get { path, .. } => path.starts_with('/'),
                    FileRequest::Put { dir } => dir.starts_with('/'),
                }) {
                    self.events.push(TermEvent::FileRequest(request));
                }
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
            _ => tracing::debug!(
                "unhandled OSC {}",
                params.iter().map(|p| String::from_utf8_lossy(p)).collect::<Vec<_>>().join(";")
            ),
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
            // XTSMGRAPHICS: sixel colour registers (1) and maximum size (2).
            ([b'?'], 'S') => {
                let (item, action) = (arg(0, 0), arg(1, 0));
                let reply = match (item, action) {
                    (1, 1 | 4) => "1;0;1024".to_string(),
                    (2, 1 | 4) => {
                        let (w, h) = (self.cell_width as usize * self.cols, self.cell_height as usize * self.rows);
                        format!("2;0;{w};{h}")
                    }
                    _ => format!("{item};1"),
                };
                self.reply(&format!("\x1b[?{reply}S"));
            }
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
                // VT220 with ANSI colour and clipboard access (and sixel graphics).
                if self.advertise_sixel {
                    self.reply("\x1b[?62;4;22;52c");
                } else {
                    self.reply("\x1b[?62;22;52c");
                }
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

/// The command line in OSC 133;C's parameters: `cmdline_url=` (percent
/// encoded, fish) or `cmdline=` (as is; it may contain `;`, so the rest of
/// the parameters belong to it).
fn command_line(params: &[&[u8]]) -> Option<String> {
    for (i, param) in params.iter().enumerate() {
        if let Some(url) = param.strip_prefix(b"cmdline_url=") {
            return Some(percent_decode(url));
        }
        if let Some(raw) = param.strip_prefix(b"cmdline=") {
            let mut text = String::from_utf8_lossy(raw).into_owned();
            for rest in &params[i + 1..] {
                text.push(';');
                text.push_str(&String::from_utf8_lossy(rest));
            }
            return Some(text);
        }
    }
    None
}

fn percent_decode(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The last percentage in a line of text, like `45%` or `12.5 %`.
fn last_percentage(text: &str) -> Option<f32> {
    let chars: Vec<char> = text.chars().collect();
    let mut found = None;
    for (i, &c) in chars.iter().enumerate() {
        if c != '%' {
            continue;
        }
        let mut start = i;
        if start > 0 && chars[start - 1] == ' ' {
            start -= 1;
        }
        let end = start;
        while start > 0 && (chars[start - 1].is_ascii_digit() || chars[start - 1] == '.') {
            start -= 1;
        }
        let number: String = chars[start..end].iter().collect();
        if let Ok(value) = number.trim_start_matches('.').parse::<f32>()
            && (0.0..=100.0).contains(&value)
            && !number.is_empty()
        {
            found = Some(value);
        }
    }
    found
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

    /// Two commands as fish would mark them: one with output that failed,
    /// one with none, then a third prompt.
    fn session(t: &mut Term, p: &mut Processor) {
        p.advance(t, b"\x1b]133;A;click_events=1\x07$ \x1b]133;B\x07false\r\n");
        p.advance(t, b"\x1b]133;C\x07out 1\r\nout 2\r\n\x1b]133;D;1\x07");
        p.advance(t, b"\x1b]133;A\x07$ \x1b]133;B\x07true\r\n\x1b]133;C\x07\x1b]133;D;0\x07");
        p.advance(t, b"\x1b]133;A;click_events=1\x07$ ");
    }

    #[test]
    fn marks_commands_and_their_output() {
        let (mut t, mut p) = term(20, 10);
        session(&mut t, &mut p);
        // `true` wrote nothing, so the next prompt is on its output's row.
        assert_eq!(t.prompt_lines(), vec![0, 3, 4]);
        assert_eq!(t.grid[0].status, Some(1));
        assert_eq!(t.grid[3].status, Some(0));
        assert_eq!(t.failed_commands_in_view(), vec![(0, 1)]);
        // Clicking the output or the command line picks the same output.
        assert_eq!(t.command_output_at(2), Some((1, 2)));
        assert_eq!(t.command_output_at(0), Some((1, 2)));
        // `true` wrote nothing; the last output is still `false`'s.
        assert_eq!(t.command_output_at(3), None);
        assert_eq!(t.last_command_output(), Some((1, 2)));
        assert_eq!(t.text_between((1, 0), (2, 19)), "out 1\nout 2");
        // The command line being edited takes clicks; old output doesn't.
        assert!(t.click_goes_to_shell(4));
        assert!(!t.click_goes_to_shell(1));
        // A running command's output counts too, up to the cursor.
        p.advance(&mut t, b"sleep\r\n\x1b]133;C\x07zzz");
        assert!(!t.click_goes_to_shell(4));
        assert_eq!(t.last_command_output(), Some((5, 5)));
    }

    #[test]
    fn marks_survive_reflow() {
        let (mut t, mut p) = term(20, 10);
        session(&mut t, &mut p);
        t.resize(8, 10);
        let prompts = t.prompt_lines();
        assert_eq!(prompts.len(), 3);
        assert_eq!(t.line(prompts[0]).and_then(|r| r.status), Some(1));
        assert!(t.last_command_output().is_some());
    }

    #[test]
    fn reports_progress() {
        let (mut t, mut p) = term(30, 4);
        p.advance(&mut t, b"\x1b]9;4;1;42\x07\x1b]9;4;3\x07\x1b]9;4;0\x07\x1b]9;hello\x07");
        let progress: Vec<_> = t
            .take_events()
            .into_iter()
            .filter_map(|e| if let TermEvent::Progress(p) = e { Some(p) } else { None })
            .collect();
        assert_eq!(
            progress,
            vec![
                Some(Progress {
                    state: ProgressState::Normal,
                    percent: Some(42)
                }),
                Some(Progress {
                    state: ProgressState::Indeterminate,
                    percent: None
                }),
                None,
            ]
        );
        // A line rewritten in place with a percentage; a finished line isn't.
        p.advance(&mut t, b"\x1b]133;C\x07done 100%\r\n");
        assert_eq!(t.progress_hint(), None);
        p.advance(&mut t, b"[###   ]  45.5%\r[####  ]  62%");
        assert_eq!(t.progress_hint(), Some(62.0));
        // Back at the prompt, a battery level isn't progress.
        p.advance(&mut t, b"\r\n\x1b]133;A\x07bat 80% $ \r");
        assert_eq!(t.progress_hint(), None);
    }

    #[test]
    fn reports_commands_and_notifications() {
        let (mut t, mut p) = term(30, 4);
        p.advance(&mut t, b"\x1b]133;A\x07$ \x1b]133;C;cmdline_url=make%20-j8\x07");
        p.advance(&mut t, b"\x1b]133;D;2\x07\x1b]133;C;cmdline=ls;cd x\x07\x1b]133;D;0\x07");
        p.advance(&mut t, b"\x1b]777;notify;Build;all done\x07\x1b]9;ping\x07");
        let events: Vec<_> = t.take_events().into_iter().filter(|e| !matches!(e, TermEvent::Prompt)).collect();
        assert_eq!(
            events,
            vec![
                TermEvent::CommandStarted(Some("make -j8".into())),
                TermEvent::CommandFinished(Some(2)),
                TermEvent::CommandStarted(Some("ls;cd x".into())),
                TermEvent::CommandFinished(Some(0)),
                TermEvent::Notify {
                    title: "Build".into(),
                    body: "all done".into()
                },
                TermEvent::Notify {
                    title: String::new(),
                    body: "ping".into()
                },
            ]
        );
    }

    #[test]
    fn finds_percentages() {
        assert_eq!(last_percentage("12% then 45.5 %  "), Some(45.5));
        assert_eq!(last_percentage("100%"), Some(100.0));
        assert_eq!(last_percentage("250% and %"), None);
        assert_eq!(last_percentage("no numbers"), None);
    }

    #[test]
    fn file_requests_need_absolute_paths() {
        let (mut t, mut p) = term(20, 3);
        // "/etc/hostname", "/srv/my dir", and a relative "etc".
        p.advance(&mut t, b"\x1b]5379;get;L2V0Yy9ob3N0bmFtZQ==\x07\x1b]5379;put;L3Nydi9teSBkaXI=\x1b\\");
        p.advance(&mut t, b"\x1b]5379;get-dir;ZXRj\x07\x1b]5379;run;L2Jpbg==\x07");
        assert_eq!(
            t.take_events(),
            vec![
                TermEvent::FileRequest(FileRequest::Get {
                    path: "/etc/hostname".into(),
                    dir: false
                }),
                TermEvent::FileRequest(FileRequest::Put { dir: "/srv/my dir".into() }),
            ]
        );
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
    fn hyperlinks_survive_sgr_reset() {
        let (mut t, mut p) = term(20, 2);
        p.advance(&mut t, b"\x1b]8;;https://ionnet.io\x1b\\a\x1b[0mb\x1b]8;;\x1b\\c");
        let cells = &t.grid[0].cells;
        assert_eq!(t.hyperlink(cells[0].hyperlink), Some("https://ionnet.io"));
        assert_eq!(cells[1].hyperlink, cells[0].hyperlink);
        assert_eq!(cells[2].hyperlink, 0);
    }

    #[test]
    fn prompt_is_blanked_not_reflowed() {
        let (mut t, mut p) = term(10, 5);
        // Output, then a two-line prompt whose first line fills the width.
        p.advance(&mut t, b"out\r\n\x1b]133;A\x07left  rght\r\n> \x1b]133;B\x07");
        t.resize(6, 5);
        assert_eq!(line(&t, 0), "out");
        assert_eq!((line(&t, 1), line(&t, 2)), (String::new(), String::new()));
        // The cursor keeps its offset from the prompt start, so the shell's
        // redraw (up one row, clear, print) lands on the blanked rows.
        assert_eq!(t.cursor.row, 2);
        assert!(t.grid[1].prompt);
        // Once a command runs, resizing reflows normally again.
        p.advance(&mut t, b"\x1b]133;C\x07");
        t.resize(10, 5);
        assert_eq!(line(&t, 0), "out");
    }

    #[test]
    fn kitty_graphics_round_trip() {
        let (mut t, mut p) = term(20, 5);
        t.set_cell_size(10, 20);
        // A 2x1 RGB image, transmitted and displayed, split over two chunks
        // and two reads (the second read starts mid-escape).
        p.advance(&mut t, b"\x1b_Ga=T,f=24,s=2,v=1,i=5,m=1;AAAA\x1b\\\x1b");
        p.advance(&mut t, b"_Gm=0;AAAA\x1b\\after");
        assert_eq!(t.take_output(), b"\x1b_Gi=5;OK\x1b\\");
        let snap = t.snapshot();
        assert_eq!(snap.images.len(), 1);
        assert_eq!(snap.images[0].image.width, 2);
        // The cursor moved past the image (one cell wide), and normal text still parses.
        assert_eq!(line(&t, 0), " after");

        // Queries answer without storing anything.
        p.advance(&mut t, b"\x1b_Ga=q,i=31,s=1,v=1,f=24;AAAA\x1b\\");
        assert_eq!(t.take_output(), b"\x1b_Gi=31;OK\x1b\\");
        // Deleting by id removes the placement.
        p.advance(&mut t, b"\x1b_Ga=d,d=I,i=5\x1b\\");
        assert!(t.snapshot().images.is_empty());
    }

    #[test]
    fn sixel_images_are_placed() {
        let (mut t, mut p) = term(20, 5);
        t.set_cell_size(10, 20);
        // 12 columns of red, 6 pixels tall: one cell row, two columns wide.
        p.advance(&mut t, b"ab\x1bP0;1;0q#1;2;100;0;0!12~\x1b\\cd");
        let snap = t.snapshot();
        assert_eq!(snap.images.len(), 1);
        assert_eq!((snap.images[0].image.width, snap.images[0].image.height), (12, 6));
        assert_eq!((snap.images[0].row, snap.images[0].col), (0, 2));
        // The cursor went to the line below, at the image's column.
        assert_eq!(line(&t, 1), "  cd");
        p.advance(&mut t, b"\x1b[c\x1b[?2;1S");
        assert_eq!(t.take_output(), b"\x1b[?62;22;52c\x1b[?2;0;200;100S");
        t.set_advertise_sixel(true);
        p.advance(&mut t, b"\x1b[c");
        assert_eq!(t.take_output(), b"\x1b[?62;4;22;52c");
    }

    #[test]
    fn resize_reflows_long_lines() {
        let (mut t, mut p) = term(10, 4);
        p.advance(&mut t, b"$ echo abcdefghijkl\r\nabcdefghijkl\r\n$ ");
        // 10 columns: "$ echo abc" / "defghijkl" / "abcdefghij" / "kl" / "$ "
        t.resize(20, 4);
        assert_eq!(line(&t, 0), "$ echo abcdefghijkl");
        assert_eq!(line(&t, 1), "abcdefghijkl");
        assert_eq!(line(&t, 2), "$");
        assert_eq!((t.cursor.row, t.cursor.col), (2, 2));
        t.resize(6, 4);
        // Narrower again: earlier rows move into the scrollback.
        assert_eq!(line(&t, 3), "$");
        assert_eq!((t.cursor.row, t.cursor.col), (3, 2));
        assert_eq!(t.text_between((0, 0), (t.history_len() + 2, 5)), "$ echo abcdefghijkl\nabcdefghijkl");
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
