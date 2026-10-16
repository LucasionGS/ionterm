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
use crate::pty::{OutputSink, Pty, SharedWriter};
use crate::ssh::{Connection, DetectedSsh, detect};
use crate::zmodem;

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
    /// The remote shell's directory, from OSC 7 sent by another host.
    pub remote_cwd: Option<String>,
    /// Set when ionterm opened this pane as an SSH connection.
    pub connection: Option<Connection>,
    /// An `ssh` the user typed, recognised from the foreground process.
    pub detected_ssh: Option<DetectedSsh>,
    /// The exit code of a connection that ended badly, while the pane offers
    /// to reconnect.
    pub disconnected: Option<u32>,
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
    /// ZMODEM state shared with the PTY reader.
    pub zmodem: Arc<zmodem::Shared>,
    /// Progress shown in the tab, while there is some.
    pub progress: Option<PaneProgress>,
    /// The command running (shell integration): when it started, and its
    /// command line when the shell says.
    pub command: Option<(Instant, Option<String>)>,
    /// It was started for a program (`-e`, an editor, a pager) rather than
    /// the shell, so it isn't reopened with the session.
    pub runs_command: bool,
    /// Port forwards over its connection; cancelled when the pane goes.
    pub forwards: crate::forward::Forwards,
    /// TCP ports its programs listen on (local panes), refreshed while its
    /// tab shows.
    pub listening: Vec<u16>,
    /// When the view last scrolled, so the scrollbar shows for a moment.
    pub scrolled_at: Option<Instant>,
}

/// A pane's progress: reported by the program (OSC 9;4), or read from a
/// percentage on a line it keeps rewriting.
#[derive(Clone, Copy, Debug)]
pub struct PaneProgress {
    pub shown: crate::tabbar::TabProgress,
    /// Read from the output rather than reported: it lapses when the
    /// percentage stops changing for a while.
    pub guessed: bool,
    pub updated: Instant,
}

impl Pane {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        id: PaneId,
        config: &TerminalConfig,
        connection: Option<Connection>,
        cwd: Option<PathBuf>,
        palette: Palette,
        metrics: CellMetrics,
        proxy: EventLoopProxy<UserEvent>,
        zmodem: Arc<zmodem::Shared>,
    ) -> anyhow::Result<Self> {
        // A placeholder size; the first layout pass sets the real one.
        let (cols, rows) = (80, 24);
        let mut term = Term::new(cols, rows, config.scrollback_lines, palette);
        term.set_cell_size(metrics.width as u32, metrics.height as u32);
        term.set_advertise_sixel(config.advertise_sixel);
        let term = Arc::new(Mutex::new(term));
        let wakeup_pending = Arc::new(AtomicBool::new(false));
        let pty = Pty::spawn(
            config,
            connection.as_ref().map(|c| c.argv.as_slice()),
            cwd.as_deref(),
            pty_size(cols, rows, metrics),
            OutputSink {
                pane: id,
                term: term.clone(),
                proxy,
                wakeup_pending: wakeup_pending.clone(),
                zmodem: zmodem.clone(),
            },
        )?;
        let forwards = crate::forward::Forwards::new(connection.clone().filter(|c| c.managed));
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
            remote_cwd: None,
            connection,
            detected_ssh: None,
            disconnected: None,
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
            zmodem,
            progress: None,
            command: None,
            runs_command: !config.command.is_empty(),
            forwards,
            listening: Vec::new(),
            scrolled_at: None,
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, Term> {
        self.term.lock().expect("terminal lock poisoned")
    }

    /// Sends input to the program. Dropped while a ZMODEM transfer runs:
    /// stray keys would corrupt it.
    pub fn write(&self, bytes: &[u8]) {
        if self.zmodem.is_active() {
            return;
        }
        self.pty.write(bytes);
    }

    /// Sends bytes even during a transfer (to cancel or nudge it).
    pub fn write_raw(&self, bytes: &[u8]) {
        self.pty.write(bytes);
    }

    pub fn pty_writer(&self) -> SharedWriter {
        self.pty.writer()
    }

    /// Looks up the foreground program again if the last check is stale.
    /// Returns whether it changed.
    pub fn refresh_process(&mut self, now: Instant) -> bool {
        if self.process_checked.is_some_and(|at| now.duration_since(at) < PROCESS_REFRESH) {
            return false;
        }
        self.process_checked = Some(now);
        let process = self.pty.foreground_process();
        let mut changed = process != self.process;
        // A typed `ssh`: remember where it went, for the tab and duplicating.
        if changed && self.connection.is_none() {
            let detected = (process.as_deref() == Some("ssh"))
                .then(|| self.pty.foreground_pid())
                .flatten()
                .and_then(detect::process_argv)
                .and_then(|argv| detect::parse_ssh_argv(&argv));
            changed |= detected != self.detected_ssh;
            self.detected_ssh = detected;
        }
        self.process = process;
        changed
    }

    /// Whether a just-exited pane should stay open offering to reconnect:
    /// connections ionterm opened that ended with an error. Writes the
    /// banner when so.
    pub fn check_disconnect(&mut self) -> bool {
        let Some(connection) = &self.connection else { return false };
        let code = self.pty.exit_code();
        if code.is_none_or(|c| c == 0) {
            return false;
        }
        let code = code.unwrap_or_default();
        let banner = format!(
            "\r\n\x1b[0m\x1b[7m Disconnected from {} (exit {code}) \x1b[0m Press Enter to reconnect, or ctrl+shift+w to close.\r\n",
            connection.alias
        );
        {
            let mut term = self.lock();
            term.reset_modes();
            ionterm_vt::Processor::new().advance(&mut term, banner.as_bytes());
        }
        self.disconnected = Some(code);
        true
    }

    /// Opens a disconnected pane's connection again, in the same terminal.
    pub fn reconnect(&mut self, config: &TerminalConfig, metrics: CellMetrics, proxy: EventLoopProxy<UserEvent>) -> anyhow::Result<()> {
        let Some(connection) = self.connection.clone() else { return Ok(()) };
        self.lock().scroll_to_bottom();
        self.pty = Pty::spawn(
            config,
            Some(&connection.argv),
            None,
            pty_size(self.grid.0, self.grid.1, metrics),
            OutputSink {
                pane: self.id,
                term: self.term.clone(),
                proxy,
                wakeup_pending: self.wakeup_pending.clone(),
                zmodem: self.zmodem.clone(),
            },
        )?;
        self.disconnected = None;
        Ok(())
    }

    /// Replaces the pane's program with another, in the same terminal (the
    /// settings tab runs the editor this way).
    pub fn run(
        &mut self,
        config: &TerminalConfig,
        argv: &[String],
        cwd: Option<&std::path::Path>,
        metrics: CellMetrics,
        proxy: EventLoopProxy<UserEvent>,
    ) -> anyhow::Result<()> {
        self.lock().scroll_to_bottom();
        self.pty = Pty::spawn(
            config,
            Some(argv),
            cwd,
            pty_size(self.grid.0, self.grid.1, metrics),
            OutputSink {
                pane: self.id,
                term: self.term.clone(),
                proxy,
                wakeup_pending: self.wakeup_pending.clone(),
                zmodem: self.zmodem.clone(),
            },
        )?;
        Ok(())
    }

    /// The process the pane started (its shell).
    pub fn child_pid(&self) -> Option<u32> {
        self.pty.child_pid()
    }

    /// Whether the pane's current program still runs (a "child exited"
    /// event may be about the one it replaced).
    pub fn is_running(&mut self) -> bool {
        self.pty.is_running()
    }

    /// The directory a new pane opened from this one should start in.
    pub fn working_dir(&self) -> Option<PathBuf> {
        self.pty.current_dir().or_else(|| self.cwd.clone())
    }

    /// Records an OSC 7 report: local paths and remote ones are kept apart,
    /// so a remote directory never becomes a local tab's starting point.
    /// Returns whether it was a remote directory.
    pub fn report_cwd(&mut self, url: &str, local_host: &str) -> bool {
        let host = url
            .strip_prefix("file://")
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default();
        let Some(path) = path_from_file_url(url) else { return false };
        if host.is_empty() || host == "localhost" || host.eq_ignore_ascii_case(local_host) {
            self.cwd = Some(path);
            false
        } else {
            self.remote_cwd = Some(path.to_string_lossy().into_owned());
            true
        }
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

    /// Finds the link at a cell; with `files` (ctrl held) a local file path
    /// counts too. Returns whether the hovered link changed.
    pub fn update_hover(&mut self, cell: Option<(usize, usize)>, files: bool) -> bool {
        // Paths in an SSH pane are on the server, where they can't be checked.
        let files = files && self.connection.is_none() && self.detected_ssh.is_none();
        let cwd = files.then(|| self.working_dir()).flatten();
        let hovered = cell.and_then(|(col, row)| {
            let term = self.lock();
            let line = term.viewport_to_absolute(row);
            links::link_at(&term, line, col).or_else(|| {
                let exists = |path: &str| crate::hints::resolve(path, cwd.as_deref()).exists();
                files.then(|| links::path_at(&term, line, col, &exists)).flatten()
            })
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
        let to_viewport = |start: (usize, usize), end: (usize, usize)| Selection {
            start: (term.absolute_to_viewport(start.0), start.1),
            end: (term.absolute_to_viewport(end.0), end.1),
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
