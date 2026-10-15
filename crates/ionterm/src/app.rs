//! The winit application: owns the window, renderer, terminal and PTY, and
//! routes events between them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Context;
use ionterm_render::{FontOptions, FrameState, LinkHighlight, RenderStyle, Renderer, Selection};
use ionterm_vt::{CursorShape, Term, TermEvent, TermMode};
use portable_pty::PtySize;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{CursorIcon, Window, WindowId};

use crate::config::{CONFIG_FILE, Config, FontConfig, CursorShapeConfig, Decorations, THEME_FILE, config_dir};
use crate::input::{self, Action, KeyCombo, MouseAction, MouseButton};
use crate::links::{self, Link, LinkKind};
use crate::pty::Pty;
use crate::theme::Theme;

/// Longest a synchronized update (mode 2026) may hold back a frame.
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
/// Editors save in several steps; wait for them to settle before reloading.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(250);
const MULTI_CLICK: Duration = Duration::from_millis(400);
/// One line per tick while a selection is dragged past the top or bottom.
const AUTOSCROLL_INTERVAL: Duration = Duration::from_millis(40);
const MIN_FONT_SIZE: f32 = 4.0;

#[derive(Debug)]
pub enum UserEvent {
    /// The PTY reader parsed new output.
    Wakeup,
    ChildExited,
    ConfigChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionKind {
    Simple,
    Word,
    Line,
}

/// A selection in absolute line coordinates, so it stays put while scrolling.
#[derive(Clone, Copy, Debug)]
struct SelectionState {
    anchor: (usize, usize),
    head: (usize, usize),
    kind: SelectionKind,
}

pub struct App {
    proxy: EventLoopProxy<UserEvent>,
    config: Config,
    theme: Theme,
    bindings: HashMap<KeyCombo, Action>,
    window: Option<TermWindow>,
    clipboard: Option<arboard::Clipboard>,
    reload_at: Option<Instant>,
    _watcher: Option<notify::RecommendedWatcher>,
    pub error: Option<anyhow::Error>,
}

struct TermWindow {
    window: Arc<Window>,
    renderer: Renderer,
    term: Arc<Mutex<Term>>,
    pty: Pty,
    wakeup_pending: Arc<AtomicBool>,
    grid: (usize, usize),
    scale_factor: f64,
    font_size: f32,
    modifiers: ModifiersState,
    focused: bool,
    blink_on: bool,
    next_blink: Option<Instant>,
    sync_deadline: Option<Instant>,
    cursor_pos: PhysicalPosition<f64>,
    held_button: Option<MouseButton>,
    selecting: bool,
    selection: Option<SelectionState>,
    last_click: Option<(Instant, (usize, usize), u8)>,
    /// Fractional wheel lines carried between touchpad events.
    scroll_remainder: f64,
    /// The link under the mouse; ctrl+click opens it.
    hovered: Option<Link>,
    last_autoscroll: Option<Instant>,
}

impl App {
    pub fn new(proxy: EventLoopProxy<UserEvent>, config: Config) -> Self {
        let watcher = watch_config(proxy.clone());
        Self {
            proxy,
            bindings: input::build_bindings(&config.keybindings),
            theme: Theme::load(),
            config,
            window: None,
            clipboard: None,
            reload_at: None,
            _watcher: watcher,
            error: None,
        }
    }

    fn render_style(&self, scale_factor: f64) -> RenderStyle {
        RenderStyle {
            padding: (self.config.window.padding * scale_factor as f32).round(),
            background_opacity: self.config.window.opacity.unwrap_or(self.theme.background_opacity),
            cursor_shape: match self.config.cursor.shape {
                CursorShapeConfig::Block => CursorShape::Block,
                CursorShapeConfig::Beam => CursorShape::Beam,
                CursorShapeConfig::Underline => CursorShape::Underline,
            },
            cursor_blink: self.config.cursor.blink_interval > 0.0,
            cursor_text: self.theme.cursor_text.map(|c| c.0),
            selection_foreground: self.theme.selection_foreground.map(|c| c.0),
            selection_background: self.theme.selection_background.0,
        }
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> anyhow::Result<TermWindow> {
        let attributes = Window::default_attributes()
            .with_title("ionterm")
            .with_transparent(true)
            .with_decorations(match self.config.window.decorations {
                Decorations::Full => true,
                Decorations::None => false,
                // Tiling compositors draw their own borders.
                Decorations::Auto => std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none(),
            });
        // The app id Hyprland window rules match on.
        #[cfg(all(unix, not(target_os = "macos")))]
        let attributes = {
            use winit::platform::wayland::WindowAttributesExtWayland;
            attributes.with_name("ionterm", "ionterm")
        };
        let window = Arc::new(event_loop.create_window(attributes).context("creating the window")?);
        window.set_cursor(CursorIcon::Text);

        let scale_factor = window.scale_factor();
        let font_size = self.config.font.size;
        let size = window.inner_size();
        let renderer = Renderer::new(
            window.clone(),
            size.width,
            size.height,
            font_options(&self.config.font, font_size, scale_factor),
            self.render_style(scale_factor),
        )?;

        // Ask for the configured size in cells; tiling compositors may ignore it.
        let m = renderer.cell_metrics();
        let pad = (self.config.window.padding * scale_factor as f32).round();
        let wanted = PhysicalSize::new(
            (f32::from(self.config.window.columns) * m.width + 2.0 * pad) as u32,
            (f32::from(self.config.window.lines) * m.height + 2.0 * pad) as u32,
        );
        let size = window.request_inner_size(wanted).unwrap_or(size);

        let (cols, rows) = renderer.grid_size(size.width, size.height);
        let mut term = Term::new(cols, rows, self.config.terminal.scrollback_lines, self.theme.palette());
        term.set_cell_size(m.width as u32, m.height as u32);
        let term = Arc::new(Mutex::new(term));
        let wakeup_pending = Arc::new(AtomicBool::new(false));
        let pty = Pty::spawn(
            &self.config.terminal,
            pty_size(cols, rows, &renderer),
            term.clone(),
            self.proxy.clone(),
            wakeup_pending.clone(),
        )?;

        let blink = self.blink_interval();
        Ok(TermWindow {
            window,
            renderer,
            term,
            pty,
            wakeup_pending,
            grid: (cols, rows),
            scale_factor,
            font_size,
            modifiers: ModifiersState::empty(),
            focused: true,
            blink_on: true,
            next_blink: blink.map(|d| Instant::now() + d),
            sync_deadline: None,
            cursor_pos: PhysicalPosition::new(0.0, 0.0),
            held_button: None,
            selecting: false,
            selection: None,
            last_click: None,
            scroll_remainder: 0.0,
            hovered: None,
            last_autoscroll: None,
        })
    }

    fn blink_interval(&self) -> Option<Duration> {
        let secs = self.config.cursor.blink_interval;
        (secs > 0.0).then(|| Duration::from_secs_f32(secs))
    }

    fn reload_config(&mut self) {
        let config = Config::load();
        let theme = Theme::load();
        if config == self.config && theme == self.theme {
            return;
        }
        tracing::info!("configuration reloaded");
        let font_changed = config.font != self.config.font;
        self.bindings = input::build_bindings(&config.keybindings);
        self.config = config;
        self.theme = theme;

        let Some(mut w) = self.window.take() else { return };
        if font_changed {
            w.font_size = self.config.font.size;
        }
        w.renderer.set_style(self.render_style(w.scale_factor));
        w.renderer.set_font(font_options(&self.config.font, w.font_size, w.scale_factor));
        {
            let mut term = w.lock();
            term.set_palette(self.theme.palette());
            term.set_scrollback_limit(self.config.terminal.scrollback_lines);
        }
        w.next_blink = self.blink_interval().map(|d| Instant::now() + d);
        w.blink_on = true;
        w.fit_grid();
        self.window = Some(w);
    }

    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(clipboard) => self.clipboard = Some(clipboard),
                Err(error) => tracing::warn!("clipboard unavailable: {error}"),
            }
        }
        self.clipboard.as_mut()
    }

    fn set_clipboard(&mut self, text: String, primary: bool) {
        let Some(clipboard) = self.clipboard() else { return };
        #[cfg(target_os = "linux")]
        let result = {
            use arboard::{LinuxClipboardKind, SetExtLinux};
            let kind = if primary { LinuxClipboardKind::Primary } else { LinuxClipboardKind::Clipboard };
            clipboard.set().clipboard(kind).text(text)
        };
        #[cfg(not(target_os = "linux"))]
        let result = if primary { Ok(()) } else { clipboard.set_text(text) };
        if let Err(error) = result {
            tracing::warn!("could not set the clipboard: {error}");
        }
    }

    fn get_clipboard(&mut self, primary: bool) -> Option<String> {
        let clipboard = self.clipboard()?;
        #[cfg(target_os = "linux")]
        let result = {
            use arboard::{GetExtLinux, LinuxClipboardKind};
            let kind = if primary { LinuxClipboardKind::Primary } else { LinuxClipboardKind::Clipboard };
            clipboard.get().clipboard(kind).text()
        };
        #[cfg(not(target_os = "linux"))]
        let result = {
            let _ = primary;
            clipboard.get_text()
        };
        result.ok()
    }

    fn paste(&mut self, primary: bool) {
        let Some(text) = self.get_clipboard(primary) else { return };
        let Some(w) = &mut self.window else { return };
        let mode = w.lock().mode();
        w.pty.write(&input::encode_paste(&text, mode));
        w.lock().scroll_to_bottom();
    }

    fn copy_selection(&mut self, primary: bool) {
        let Some(text) = self.window.as_ref().and_then(TermWindow::selected_text) else {
            return;
        };
        if !text.is_empty() {
            self.set_clipboard(text, primary);
        }
    }

    fn perform(&mut self, action: Action) {
        match action {
            Action::Copy => self.copy_selection(false),
            Action::Paste => self.paste(false),
            Action::PasteSelection => self.paste(cfg!(target_os = "linux")),
            Action::ReloadConfig => self.reload_config(),
            Action::None => {}
            _ => {
                let font = self.config.font.clone();
                let default_size = self.config.font.size;
                let Some(w) = &mut self.window else { return };
                let page = w.grid.1 as isize;
                match action {
                    Action::ScrollLineUp => w.lock().scroll_display(1),
                    Action::ScrollLineDown => w.lock().scroll_display(-1),
                    Action::ScrollPageUp => w.lock().scroll_display(page),
                    Action::ScrollPageDown => w.lock().scroll_display(-page),
                    Action::ScrollHome => w.lock().scroll_display(isize::MAX / 2),
                    Action::ScrollEnd => w.lock().scroll_to_bottom(),
                    Action::IncreaseFontSize => w.font_size += 1.0,
                    Action::DecreaseFontSize => w.font_size = (w.font_size - 1.0).max(MIN_FONT_SIZE),
                    Action::ResetFontSize => w.font_size = default_size,
                    _ => {}
                }
                if matches!(
                    action,
                    Action::IncreaseFontSize | Action::DecreaseFontSize | Action::ResetFontSize
                ) {
                    let options = font_options(&font, w.font_size, w.scale_factor);
                    w.renderer.set_font(options);
                    w.fit_grid();
                }
                w.window.request_redraw();
            }
        }
    }

    fn handle_term_events(&mut self) {
        let Some(w) = &mut self.window else { return };
        w.wakeup_pending.store(false, Ordering::Release);
        let (events, sync_started) = {
            let mut term = w.lock();
            (term.take_events(), term.sync_started())
        };
        match sync_started {
            Some(start) if start.elapsed() < SYNC_TIMEOUT => w.sync_deadline = Some(start + SYNC_TIMEOUT),
            _ => {
                w.sync_deadline = None;
                w.window.request_redraw();
            }
        }
        // The clipboard needs `&mut self`, so it's applied after the loop.
        let mut clipboard = None;
        for event in events {
            match event {
                TermEvent::Title(title) => {
                    let title = if title.is_empty() { "ionterm".to_string() } else { title };
                    w.window.set_title(&title);
                }
                TermEvent::Bell => {
                    if !w.focused {
                        w.window.request_user_attention(Some(winit::window::UserAttentionType::Informational));
                    }
                }
                TermEvent::ClipboardStore(text) => clipboard = Some(text),
                TermEvent::Cwd(_) => {}
                TermEvent::SyncEnd => w.window.request_redraw(),
            }
        }
        if let Some(text) = clipboard {
            self.set_clipboard(text, false);
        }
    }

    fn handle_mouse_button(&mut self, button: winit::event::MouseButton, state: ElementState) {
        let Some(w) = &mut self.window else { return };
        let button = match button {
            winit::event::MouseButton::Left => MouseButton::Left,
            winit::event::MouseButton::Middle => MouseButton::Middle,
            winit::event::MouseButton::Right => MouseButton::Right,
            _ => return,
        };
        let pressed = state == ElementState::Pressed;
        let (col, row) = w.cell_under_pointer();
        let mode = w.lock().mode();
        // Shift bypasses mouse reporting so text can still be selected.
        let grabbed = mode.intersects(TermMode::MOUSE_REPORTING) && !w.modifiers.shift_key();

        if !grabbed
            && pressed
            && button == MouseButton::Left
            && w.modifiers.control_key()
            && let Some(link) = &w.hovered
        {
            links::open(&link.uri);
            return;
        }

        if grabbed {
            let action = if pressed { MouseAction::Press } else { MouseAction::Release };
            if let Some(bytes) = input::encode_mouse(action, Some(button), col, row, w.modifiers, mode) {
                w.pty.write(&bytes);
            }
            w.held_button = pressed.then_some(button);
            return;
        }
        w.held_button = pressed.then_some(button);

        match (button, pressed) {
            (MouseButton::Left, true) => {
                let absolute = (w.lock().viewport_to_absolute(row), col);
                let count = match w.last_click {
                    Some((at, pos, n)) if pos == absolute && at.elapsed() < MULTI_CLICK => n % 3 + 1,
                    _ => 1,
                };
                w.last_click = Some((Instant::now(), absolute, count));
                w.selecting = true;
                w.selection = match count {
                    1 => None,
                    2 => Some(SelectionState {
                        anchor: absolute,
                        head: absolute,
                        kind: SelectionKind::Word,
                    }),
                    _ => Some(SelectionState {
                        anchor: absolute,
                        head: absolute,
                        kind: SelectionKind::Line,
                    }),
                };
                w.window.request_redraw();
            }
            (MouseButton::Left, false) => {
                w.selecting = false;
                if w.selection.is_some() {
                    self.copy_selection(true);
                }
            }
            (MouseButton::Middle, true) => self.paste(cfg!(target_os = "linux")),
            _ => {}
        }
    }

    fn handle_cursor_moved(&mut self, position: PhysicalPosition<f64>) {
        let Some(w) = &mut self.window else { return };
        let before = w.cell_under_pointer();
        w.cursor_pos = position;
        let (col, row) = w.cell_under_pointer();
        if (col, row) == before {
            return;
        }
        w.update_hover();
        let mode = w.lock().mode();
        if mode.intersects(TermMode::MOUSE_REPORTING) && !w.modifiers.shift_key() {
            let action = MouseAction::Motion(w.held_button);
            if let Some(bytes) = input::encode_mouse(action, None, col, row, w.modifiers, mode) {
                w.pty.write(&bytes);
            }
            return;
        }
        if w.selecting {
            let absolute = (w.lock().viewport_to_absolute(row), col);
            match &mut w.selection {
                Some(selection) => selection.head = absolute,
                None => {
                    let anchor = w.last_click.map_or(absolute, |(_, pos, _)| pos);
                    w.selection = Some(SelectionState {
                        anchor,
                        head: absolute,
                        kind: SelectionKind::Simple,
                    });
                }
            }
            w.window.request_redraw();
        }
    }

    fn handle_wheel(&mut self, delta: MouseScrollDelta) {
        let wheel_lines = self.config.terminal.wheel_scroll_lines as f64;
        let Some(w) = &mut self.window else { return };
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => f64::from(y) * wheel_lines,
            MouseScrollDelta::PixelDelta(p) => p.y / f64::from(w.renderer.cell_metrics().height),
        } + w.scroll_remainder;
        let whole = lines.trunc() as isize;
        w.scroll_remainder = lines.fract();
        if whole == 0 {
            return;
        }

        let (col, row) = w.cell_under_pointer();
        let mode = w.lock().mode();
        if mode.intersects(TermMode::MOUSE_REPORTING) {
            let button = if whole > 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
            for _ in 0..whole.unsigned_abs() {
                if let Some(bytes) = input::encode_mouse(MouseAction::Press, Some(button), col, row, w.modifiers, mode) {
                    w.pty.write(&bytes);
                }
            }
        } else if mode.contains(TermMode::ALT_SCREEN) {
            // Full-screen apps without mouse support (less, man) get arrow keys.
            let key: &[u8] = match (whole > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            w.pty.write(&key.repeat(whole.unsigned_abs()));
        } else {
            w.lock().scroll_display(whole);
            w.update_hover();
            w.window.request_redraw();
        }
    }
}

impl TermWindow {
    fn lock(&self) -> MutexGuard<'_, Term> {
        self.term.lock().expect("terminal lock poisoned")
    }

    fn cell_under_pointer(&self) -> (usize, usize) {
        self.renderer
            .cell_at(self.cursor_pos.x, self.cursor_pos.y, self.grid.0, self.grid.1)
    }

    /// Finds the link under the mouse and shows it as hovered.
    fn update_hover(&mut self) {
        let (col, row) = self.cell_under_pointer();
        let hovered = {
            let term = self.lock();
            links::link_at(&term, term.viewport_to_absolute(row), col)
        };
        if hovered != self.hovered {
            self.window.set_cursor(if hovered.is_some() { CursorIcon::Pointer } else { CursorIcon::Text });
            self.hovered = hovered;
            self.window.request_redraw();
        }
    }

    /// Scrolls while a selection is dragged past the top or bottom of the
    /// grid, extending it. Returns when to check again.
    fn autoscroll(&mut self, now: Instant) -> Option<Instant> {
        if !self.selecting || self.selection.is_none() {
            self.last_autoscroll = None;
            return None;
        }
        let (top, bottom) = self.renderer.grid_vertical_bounds(self.grid.1);
        let y = self.cursor_pos.y as f32;
        let delta: isize = if y < top {
            1
        } else if y >= bottom {
            -1
        } else {
            self.last_autoscroll = None;
            return None;
        };
        let due = self.last_autoscroll.is_none_or(|at| now.duration_since(at) >= AUTOSCROLL_INTERVAL);
        if due {
            let (col, _) = self.cell_under_pointer();
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
            self.window.request_redraw();
        }
        self.last_autoscroll.map(|at| at + AUTOSCROLL_INTERVAL)
    }

    /// Recomputes the grid after a size, scale or font change and tells the
    /// terminal and the PTY.
    fn fit_grid(&mut self) {
        let size = self.window.inner_size();
        self.renderer.resize(size.width, size.height);
        let (cols, rows) = self.renderer.grid_size(size.width, size.height);
        let m = self.renderer.cell_metrics();
        {
            let mut term = self.lock();
            term.set_cell_size(m.width as u32, m.height as u32);
            term.resize(cols, rows);
        }
        if (cols, rows) != self.grid {
            self.selection = None;
        }
        self.grid = (cols, rows);
        self.pty.resize(pty_size(cols, rows, &self.renderer));
        self.window.request_redraw();
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

    fn selected_text(&self) -> Option<String> {
        let term = self.lock();
        let (start, end) = self.selection_bounds(&term)?;
        Some(term.text_between(start, end))
    }

    fn redraw(&mut self) {
        let (snapshot, selection, hovered_link) = {
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
            (term.snapshot(), selection, hovered_link)
        };
        self.renderer.render(
            &snapshot,
            FrameState {
                focused: self.focused,
                cursor_blink_on: self.blink_on,
                selection,
                hovered_link,
            },
        );
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

fn font_options(font: &FontConfig, size_pt: f32, scale_factor: f64) -> FontOptions {
    FontOptions {
        family: font.family.clone(),
        ligatures: font.ligatures,
        // Points to pixels at 96 DPI, then the output's scale.
        size: size_pt * 96.0 / 72.0 * scale_factor as f32,
    }
}

fn pty_size(cols: usize, rows: usize, renderer: &Renderer) -> PtySize {
    let m = renderer.cell_metrics();
    PtySize {
        rows: rows as u16,
        cols: cols as u16,
        pixel_width: (cols as f32 * m.width) as u16,
        pixel_height: (rows as f32 * m.height) as u16,
    }
}

/// Watches the config directory (not the files: the Ionix styler replaces
/// `theme.toml` by re-linking it, which a file watch would miss).
fn watch_config(proxy: EventLoopProxy<UserEvent>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;

    let dir = config_dir()?;
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::warn!("cannot create {}: {error}", dir.display());
        return None;
    }
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else { return };
        let relevant = event.paths.iter().any(|path| {
            path.file_name()
                .is_some_and(|name| name == CONFIG_FILE || name == THEME_FILE)
        });
        if relevant {
            let _ = proxy.send_event(UserEvent::ConfigChanged);
        }
    })
    .map_err(|error| tracing::warn!("config live reload disabled: {error}"))
    .ok()?;
    if let Err(error) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
        tracing::warn!("config live reload disabled: {error}");
        return None;
    }
    Some(watcher)
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        match self.create_window(event_loop) {
            Ok(window) => self.window = Some(window),
            Err(error) => {
                self.error = Some(error);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if let Some(w) = &mut self.window {
                    w.redraw();
                }
            }
            WindowEvent::Resized(_) => {
                if let Some(w) = &mut self.window {
                    w.fit_grid();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let style = self.render_style(scale_factor);
                let Some(size) = self.window.as_ref().map(|w| w.font_size) else { return };
                let font = font_options(&self.config.font, size, scale_factor);
                if let Some(w) = &mut self.window {
                    w.scale_factor = scale_factor;
                    w.renderer.set_style(style);
                    w.renderer.set_font(font);
                    w.fit_grid();
                }
            }
            WindowEvent::Focused(focused) => {
                if let Some(w) = &mut self.window {
                    w.focused = focused;
                    w.blink_on = true;
                    if w.lock().mode().contains(TermMode::FOCUS_EVENTS) {
                        w.pty.write(if focused { b"\x1b[I" } else { b"\x1b[O" });
                    }
                    w.window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                if let Some(w) = &mut self.window {
                    w.modifiers = modifiers.state();
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => {
                let blink = self.blink_interval();
                let Some(w) = &mut self.window else { return };
                let mods = w.modifiers;
                if event.state == ElementState::Pressed
                    && let Some(action) = KeyCombo::from_event(&event, mods).and_then(|c| self.bindings.get(&c))
                {
                    let action = *action;
                    return self.perform(action);
                }
                let (mode, flags) = {
                    let term = w.lock();
                    (term.mode(), term.keyboard_flags())
                };
                if let Some(bytes) = input::encode_key(&event, mods, mode, flags) {
                    w.pty.write(&bytes);
                    if event.state == ElementState::Pressed {
                        w.lock().scroll_to_bottom();
                        w.blink_on = true;
                        w.next_blink = blink.map(|d| Instant::now() + d);
                        w.window.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => self.handle_mouse_button(button, state),
            WindowEvent::CursorMoved { position, .. } => self.handle_cursor_moved(position),
            WindowEvent::MouseWheel { delta, .. } => self.handle_wheel(delta),
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wakeup => self.handle_term_events(),
            UserEvent::ChildExited => event_loop.exit(),
            UserEvent::ConfigChanged => self.reload_at = Some(Instant::now() + RELOAD_DEBOUNCE),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if self.reload_at.is_some_and(|at| at <= now) {
            self.reload_at = None;
            self.reload_config();
        }
        let blink = self.blink_interval();
        let mut wake = self.reload_at;
        if let Some(w) = &mut self.window {
            if w.sync_deadline.is_some_and(|at| at <= now) {
                w.sync_deadline = None;
                w.window.request_redraw();
            }
            match (blink, w.focused) {
                (Some(interval), true) => {
                    let next = *w.next_blink.get_or_insert(now + interval);
                    if next <= now {
                        w.blink_on = !w.blink_on;
                        w.next_blink = Some(now + interval);
                        w.window.request_redraw();
                    }
                }
                _ => w.next_blink = None,
            }
            let autoscroll = w.autoscroll(now);
            wake = [wake, w.next_blink, w.sync_deadline, autoscroll].into_iter().flatten().min();
        }
        event_loop.set_control_flow(match wake {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
    }
}
