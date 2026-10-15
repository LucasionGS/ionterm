//! One shell: its terminal state, PTY, and everything the mouse does to it.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ionterm_render::{CellMetrics, FrameState, LinkHighlight, Selection};
use ionterm_vt::{Palette, Snapshot, Term};
use portable_pty::PtySize;
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;
use crate::config::TerminalConfig;
use crate::input::MouseButton;
use crate::layout::{PaneId, Rect};
use crate::links::{self, Link, LinkKind};
use crate::pty::Pty;

/// One line per tick while a selection is dragged past the top or bottom.
const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(40);
/// How often the foreground process (for the tab icon) is looked up again.
const PROCESS_REFRESH: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    Word,
    Line,
}

/// A selection in absolute line coordinates, so it stays put while scrolling.
#[derive(Clone, Copy, Debug)]
pub struct SelectionState {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
    pub kind: SelectionKind,
}

pub struct Pane {
    pub id: PaneId,
    term: Arc<Mutex<Term>>,
    pty: Pty,
    pub wakeup_pending: Arc<AtomicBool>,
    /// The pane's area inside its borders, padding included.
    pub area: Rect,
    /// Top-left corner of the grid.
    pub origin: [f32; 2],
    pub grid: (usize, usize),
    /// From OSC 0/2; empty until the shell sets one.
    pub title: String,
    /// From OSC 7, as a path.
    pub cwd: Option<PathBuf>,
    pub sync_deadline: Option<Instant>,
    pub held_button: Option<MouseButton>,
    pub selecting: bool,
    pub selection: Option<SelectionState>,
    pub last_click: Option<(Instant, (usize, usize), u8)>,
    /// Fractional wheel lines carried between touchpad events.
    pub scroll_remainder: f64,
    /// The link under the mouse; ctrl+click opens it.
    pub hovered: Option<Link>,
    last_autoscroll: Option<Instant>,
    /// The foreground program's name, refreshed every `PROCESS_REFRESH`.
    pub process: Option<String>,
    process_checked: Option<Instant>,
}

impl Pane {
    pub fn spawn(
        id: PaneId,
        config: &TerminalConfig,
        cwd: Option<PathBuf>,
        palette: Palette,
        metrics: CellMetrics,
        proxy: EventLoopProxy<UserEvent>,
    ) -> anyhow::Result<Self> {
        // A placeholder size; the first layout pass sets the real one.
        let (cols, rows) = (80, 24);
        let mut term = Term::new(cols, rows, config.scrollback_lines, palette);
        term.set_cell_size(metrics.width as u32, metrics.height as u32);
        let term = Arc::new(Mutex::new(term));
        let wakeup_pending = Arc::new(AtomicBool::new(false));
        let pty = Pty::spawn(
            id,
            config,
            cwd.as_deref(),
            pty_size(cols, rows, metrics),
            term.clone(),
            proxy,
            wakeup_pending.clone(),
        )?;
        Ok(Self {
            id,
            term,
            pty,
            wakeup_pending,
            area: Rect::default(),
            origin: [0.0, 0.0],
            grid: (cols, rows),
            title: String::new(),
            cwd,
            sync_deadline: None,
            held_button: None,
            selecting: false,
            selection: None,
            last_click: None,
            scroll_remainder: 0.0,
            hovered: None,
            last_autoscroll: None,
            process: None,
            process_checked: None,
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, Term> {
        self.term.lock().expect("terminal lock poisoned")
    }

    pub fn write(&self, bytes: &[u8]) {
        self.pty.write(bytes);
    }

    /// Looks up the foreground program again if the last check is stale.
    /// Returns whether it changed.
    pub fn refresh_process(&mut self, now: Instant) -> bool {
        if self.process_checked.is_some_and(|at| now.duration_since(at) < PROCESS_REFRESH) {
            return false;
        }
        self.process_checked = Some(now);
        let process = self.pty.foreground_process();
        let changed = process != self.process;
        self.process = process;
        changed
    }

    /// The directory a new pane opened from this one should start in.
    pub fn working_dir(&self) -> Option<PathBuf> {
        self.pty.current_dir().or_else(|| self.cwd.clone())
    }

    /// Moves the pane to `area` and sizes its grid to fit, telling the
    /// terminal and the shell when the size in cells changes.
    pub fn place(&mut self, area: Rect, padding: f32, m: CellMetrics, force: bool) {
        self.area = area;
        self.origin = [area.x + padding, area.y + padding];
        let cols = (((area.width - 2.0 * padding) / m.width) as usize).max(2);
        let rows = (((area.height - 2.0 * padding) / m.height) as usize).max(1);
        if (cols, rows) == self.grid && !force {
            return;
        }
        {
            let mut term = self.lock();
            term.set_cell_size(m.width as u32, m.height as u32);
            term.resize(cols, rows);
        }
        if (cols, rows) != self.grid {
            self.selection = None;
        }
        self.grid = (cols, rows);
        self.pty.resize(pty_size(cols, rows, m));
    }

    /// The cell under a point, clamped to the grid.
    pub fn cell_at(&self, x: f64, y: f64, m: CellMetrics) -> (usize, usize) {
        let col = ((x as f32 - self.origin[0]) / m.width).max(0.0) as usize;
        let row = ((y as f32 - self.origin[1]) / m.height).max(0.0) as usize;
        (col.min(self.grid.0 - 1), row.min(self.grid.1 - 1))
    }

    /// Finds the link at a cell. Returns whether the hovered link changed.
    pub fn update_hover(&mut self, cell: Option<(usize, usize)>) -> bool {
        let hovered = cell.and_then(|(col, row)| {
            let term = self.lock();
            links::link_at(&term, term.viewport_to_absolute(row), col)
        });
        let changed = hovered != self.hovered;
        self.hovered = hovered;
        changed
    }

    /// Scrolls while a selection is dragged above or below the grid,
    /// extending it. Returns when to check again, and whether to redraw.
    pub fn autoscroll(&mut self, now: Instant, pointer: (f64, f64), m: CellMetrics) -> (Option<Instant>, bool) {
        if !self.selecting || self.selection.is_none() {
            self.last_autoscroll = None;
            return (None, false);
        }
        let top = self.origin[1];
        let bottom = top + self.grid.1 as f32 * m.height;
        let y = pointer.1 as f32;
        let delta: isize = if y < top {
            1
        } else if y >= bottom {
            -1
        } else {
            self.last_autoscroll = None;
            return (None, false);
        };
        let due = self.last_autoscroll.is_none_or(|at| now.duration_since(at) >= AUTOSCROLL_INTERVAL);
        if due {
            let (col, _) = self.cell_at(pointer.0, pointer.1, m);
            let edge_row = if delta > 0 { 0 } else { self.grid.1 - 1 };
            let head = {
                let mut term = self.lock();
                term.scroll_display(delta);
                (term.viewport_to_absolute(edge_row), col)
            };
            if let Some(selection) = &mut self.selection {
                selection.head = head;
            }
            self.last_autoscroll = Some(now);
        }
        (self.last_autoscroll.map(|at| at + AUTOSCROLL_INTERVAL), due)
    }

    /// The selection's normalised bounds in absolute coordinates.
    fn selection_bounds(&self, term: &Term) -> Option<((usize, usize), (usize, usize))> {
        let s = self.selection?;
        let (mut start, mut end) = if s.anchor <= s.head { (s.anchor, s.head) } else { (s.head, s.anchor) };
        match s.kind {
            SelectionKind::Simple => {}
            SelectionKind::Word => {
                start.1 = word_edge(term, start, -1);
                end.1 = word_edge(term, end, 1);
            }
            SelectionKind::Line => {
                start.1 = 0;
                end.1 = term.cols() - 1;
            }
        }
        Some((start, end))
    }

    pub fn selected_text(&self) -> Option<String> {
        let term = self.lock();
        let (start, end) = self.selection_bounds(&term)?;
        Some(term.text_between(start, end))
    }

    /// What the renderer needs to draw this pane.
    pub fn frame(&self, focused: bool, cursor_blink_on: bool) -> (Snapshot, FrameState) {
        let term = self.lock();
        let top = term.viewport_to_absolute(0) as isize;
        let to_viewport = |start: (usize, usize), end: (usize, usize)| Selection {
            start: (start.0 as isize - top, start.1),
            end: (end.0 as isize - top, end.1),
        };
        let selection = self.selection_bounds(&term).map(|(start, end)| to_viewport(start, end));
        let hovered_link = self.hovered.as_ref().map(|link| match link.kind {
            LinkKind::Hyperlink(id) => LinkHighlight::Hyperlink(id),
            LinkKind::Text(start, end) => LinkHighlight::Span(to_viewport(start, end)),
        });
        let frame = FrameState {
            focused,
            cursor_blink_on,
            selection,
            hovered_link,
        };
        (term.snapshot(), frame)
    }
}

/// The column where the word around `pos` ends, searching in `direction`.
fn word_edge(term: &Term, pos: (usize, usize), direction: isize) -> usize {
    let Some(row) = term.line(pos.0) else { return pos.1 };
    let is_word = |c: char| c.is_alphanumeric() || "_-./~:@%+#?=&".contains(c);
    let cells = &row.cells;
    if pos.1 >= cells.len() || !is_word(cells[pos.1].c) {
        return pos.1.min(cells.len().saturating_sub(1));
    }
    let mut col = pos.1;
    loop {
        let next = col as isize + direction;
        if next < 0 || next as usize >= cells.len() || !is_word(cells[next as usize].c) {
            return col;
        }
        col = next as usize;
    }
}

fn pty_size(cols: usize, rows: usize, m: CellMetrics) -> PtySize {
    PtySize {
        rows: rows as u16,
        cols: cols as u16,
        pixel_width: (cols as f32 * m.width) as u16,
        pixel_height: (rows as f32 * m.height) as u16,
    }
}

/// Turns an OSC 7 `file://host/path` URL into a path, undoing %-escapes.
pub fn path_from_file_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = path.get(i + 1..i + 3).and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osc7_urls() {
        assert_eq!(
            path_from_file_url("file://ionix/home/ion/my%20dir"),
            Some(PathBuf::from("/home/ion/my dir"))
        );
        assert_eq!(path_from_file_url("https://x/y"), None);
    }
}
