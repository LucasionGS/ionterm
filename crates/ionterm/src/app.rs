//! The winit application: owns the windows and their tabs of panes, and
//! routes events between the window system, the panes and the renderer.

mod background;
mod blocks;
mod drag;
mod editing;
mod files;
mod forwards;
mod hints;
mod integration;
mod remote;
mod rzsz;
mod search;
mod session;
mod settings;
mod trail;
mod windows;

pub use windows::{Content, NewWindow};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ionterm_render::{Chrome, ChromeItem, FontOptions, Gpu, PaneView, RenderStyle, Renderer};
use ionterm_vt::{CursorShape, TermEvent, TermMode};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

use crate::config::{CONFIG_FILE, Config, CursorShapeConfig, FontConfig, ShowTabBar, THEME_FILE, TabBarEdge, config_dir};
use crate::dialog::{self, Answer, Dialog};
use crate::dnd::{self, DragEvent};
use crate::zmodem;
use crate::files::FilesEvent;
use crate::files::transfer::{self, TransferEvent, TransferId, Transfers};
use crate::input::{self, Action, KeyCombo, MouseAction, MouseButton};
use crate::layout::{self, Direction, PaneId, Rect};
use crate::links;
use crate::scrollback;
use crate::editor::{self, Editor, Outcome};
use crate::menu::{self, Menu, MenuCommand};
use crate::palette::{self, Palette, PaletteAction, PaletteItem, PaneTarget};
use crate::ssh::{self, Connection, HostExtras};
use crate::pane::{Pane, SelectionKind, SelectionState};
use crate::tab::Tab;
use crate::tabbar::{self, Activity, Hit, TabInfo};
use crate::textfield::TextField;
use crate::notifications;
use crate::theme::Theme;

/// Longest a synchronized update (mode 2026) may hold back a frame.
const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
/// Editors save in several steps; wait for them to settle before reloading.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(250);
const MULTI_CLICK: Duration = Duration::from_millis(400);
const MIN_FONT_SIZE: f32 = 4.0;
/// Share of a split that one resize action moves its boundary.
const RESIZE_STEP: f32 = 0.05;
/// The tab icon for SSH sessions without one of their own.
const SSH_ICON: char = '\u{F233}';
/// The tab's icon while typing goes to all its panes.
const BROADCAST_ICON: char = '\u{F0003}';
/// How far a tab must be dragged, in logical pixels, before it moves.
const TAB_DRAG_THRESHOLD: f64 = 6.0;
/// How quickly tabs glide to their slots (per second); higher is snappier.
/// At 18 a tab covers 95% of the distance in about 170 ms.
const TAB_GLIDE_RATE: f32 = 18.0;
/// How far, in logical pixels, a tab must be dragged away from the tab bar
/// (or past the window's sides) to leave its window when let go.
const TAB_DETACH_DISTANCE: f64 = 48.0;

#[derive(Debug)]
pub enum UserEvent {
    /// A pane's PTY reader parsed new output.
    Wakeup(PaneId),
    ChildExited(PaneId),
    ConfigChanged,
    /// Background file work for a panel finished.
    Files(FilesEvent),
    /// An upload or download moved along or finished.
    Transfer(TransferId, TransferEvent),
    /// Files dragged over a window (named by `dnd::surface_key`), where
    /// winit doesn't report them.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Drag(Option<usize>, DragEvent),
    /// A ZMODEM transfer in a pane moved along.
    Zmodem(PaneId, zmodem::Event),
    /// A remote control command (`ionterm @ …`).
    Remote(crate::remote::Request),
    /// Background work on a server file being edited locally.
    Edit(u64, crate::edit::Event),
    /// Background work on a pane's port forwards.
    Forward(PaneId, crate::forward::Event),
}

pub struct App {
    proxy: EventLoopProxy<UserEvent>,
    config: Config,
    theme: Theme,
    bindings: HashMap<KeyCombo, Action>,
    /// The window being worked on. Each event makes its own window this one
    /// first (`select_window`), so everything else deals with one window.
    window: Option<TermWindow>,
    /// The other windows.
    parked: Vec<TermWindow>,
    /// Windows to open once the event loop can (`about_to_wait`).
    pending_windows: Vec<NewWindow>,
    /// The GPU of the first window, which every later window draws with too.
    gpu: Option<Gpu>,
    next_window_serial: u64,
    /// The window that last had the keyboard: remote commands from outside
    /// ionterm go there.
    last_focused: Option<WindowId>,
    /// A tab let go outside its window, waiting to see where it lands.
    tab_drop: Option<windows::TabDrop>,
    /// Server files open in a local editor.
    edits: Vec<editing::EditSession>,
    next_edit_id: u64,
    /// Listens for file drops on Wayland, for every window; stopped before
    /// the display closes.
    drops: Option<dnd::Listener>,
    clipboard: Option<arboard::Clipboard>,
    reload_at: Option<Instant>,
    next_pane_id: PaneId,
    /// Set when the last window closes; the event loop exits on its next pass.
    exit_requested: bool,
    /// The first window is opened and remote control started.
    started: bool,
    /// When the session is next compared with the saved one, and what was
    /// last saved.
    session_due: Instant,
    session_saved: Option<crate::session::Session>,
    /// Whether this ionterm took the saved session over (restored it, or
    /// found none); only then does it save one.
    session_taken: bool,
    /// This machine's name, to tell remote directory reports from local ones.
    local_host: String,
    _watcher: Option<notify::RecommendedWatcher>,
    pub error: Option<anyhow::Error>,
}

struct TermWindow {
    id: WindowId,
    /// The order windows were opened in, for the session and window lists.
    serial: u64,
    /// Part of the saved session (not windows opened for one program or host).
    in_session: bool,
    /// The window's Wayland surface, to tell which window a file drop is over.
    surface: Option<usize>,
    window: Arc<Window>,
    renderer: Renderer,
    tabs: Vec<Tab>,
    active_tab: usize,
    scale_factor: f64,
    font_size: f32,
    modifiers: ModifiersState,
    focused: bool,
    blink_on: bool,
    next_blink: Option<Instant>,
    pointer: PhysicalPosition<f64>,
    /// The pane a mouse button went down in; the drag stays with it.
    capture: Option<PaneId>,
    /// The pane and cell under the mouse at the last motion event.
    pointer_cell: Option<(PaneId, (usize, usize))>,
    cursor_icon: CursorIcon,
    tab_bar: Option<Rect>,
    /// What's under the mouse in the tab bar.
    tab_hover: Hit,
    tab_drag: Option<TabDrag>,
    rename: Option<Rename>,
    menu: Option<Menu>,
    last_tab_click: Option<(Instant, usize)>,
    /// Where each tab (by id) is drawn, which glides toward its slot.
    tab_positions: HashMap<u64, f32>,
    /// The connection picker, while open.
    palette: Option<Palette>,
    /// The connection form, while open.
    editor: Option<Editor>,
    /// Per-host colours and icons from `hosts.toml`.
    ssh_extras: HashMap<String, HostExtras>,
    /// When the last animated frame was drawn; `None` when nothing moves.
    last_frame: Option<Instant>,
    /// A modal question (delete, rename, name conflicts), while open.
    dialog: Option<Dialog<files::Purpose>>,
    /// Every upload and download, running or recently finished.
    transfers: Transfers,
    /// A short message at the bottom, and when it appeared.
    toast: Option<(String, Instant)>,
    /// The transfer card button under the mouse.
    card_hover: Option<crate::files::panel::Target>,
    /// Where files are being dragged over the window, while they are.
    file_drag: Option<PhysicalPosition<f64>>,
    /// Files winit dropped one by one (X11, Windows), handed over together.
    dropped: Vec<PathBuf>,
    /// The transfer-list entry of each pane's current ZMODEM file.
    zmodem_transfers: HashMap<PaneId, TransferId>,
    /// Hints mode, while it's shown.
    hints: Option<hints::HintsMode>,
    /// The scrollback search, while open.
    search: Option<search::SearchState>,
    /// A split border being dragged with the mouse.
    split_drag: Option<layout::SplitBorder>,
    /// The streak behind a jumping cursor.
    trail: trail::Trail,
    /// When the ports local programs listen on were last looked at.
    listening_checked: Option<Instant>,
    /// Port chips are showing (they keep the tab bar up by themselves).
    chips_shown: bool,
    /// A command block's menu, while open.
    block_menu: Option<crate::blocks::BlockMenu>,
    /// The block under the mouse (its pane and prompt line), for its handle.
    block_hover: Option<(PaneId, usize)>,
    /// The mouse is on the block handle's dots.
    block_handle_hovered: bool,
    /// The pane whose scrollbar the mouse is near, or drags.
    scrollbar_hover: Option<PaneId>,
    scrollbar_drag: Option<PaneId>,
    /// The pane whose sticky header the mouse is over.
    sticky_hover: Option<PaneId>,
}

struct TabDrag {
    index: usize,
    start_x: f64,
    /// Pointer x minus the tab's left edge when the drag began.
    grab_dx: f64,
    moved: bool,
    /// Dragged far enough from the tab bar to leave the window when let go.
    outside: bool,
}

/// A tab title being edited in place.
struct Rename {
    tab: usize,
    field: TextField,
}

/// What a tab bar click asks for, run once the window borrow ends.
enum BarAction {
    NewTab,
    /// Open the connection picker below this point (its top-right corner).
    Palette(Option<(f32, f32)>),
    /// Run a picker entry (in a split when `true`).
    Run(PaletteAction, bool),
    /// Something the connection form asked for.
    Form(Outcome),
    Close(usize),
    Menu(MenuCommand, usize),
    /// The dialog was answered.
    Dialog(Answer),
    Settings,
    /// A tab was let go away from the tab bar: it leaves the window.
    DetachTab(usize),
    /// A port chip was clicked (`true` = right or middle: stop it).
    Chip(usize, bool),
}

/// Work that needs `&mut App` after a window borrow ends.
enum FollowUp {
    None,
    CopyPrimary,
    PastePrimary,
    /// A ctrl+clicked file path, from a pane in `cwd`.
    OpenFile(links::FileLink, Option<PathBuf>),
}

impl App {
    pub fn new(proxy: EventLoopProxy<UserEvent>, config: Config, first: NewWindow) -> Self {
        let watcher = watch_config(proxy.clone());
        Self {
            proxy,
            bindings: input::build_bindings(&config.keybindings),
            theme: Theme::load(),
            config,
            window: None,
            parked: Vec::new(),
            pending_windows: vec![first],
            gpu: None,
            next_window_serial: 0,
            last_focused: None,
            tab_drop: None,
            edits: Vec::new(),
            next_edit_id: 1,
            drops: None,
            clipboard: None,
            reload_at: None,
            next_pane_id: 1,
            exit_requested: false,
            started: false,
            session_due: Instant::now(),
            session_saved: None,
            session_taken: false,
            local_host: ssh::local_hostname(),
            _watcher: watcher,
            error: None,
        }
    }

    fn render_style(&self) -> RenderStyle {
        RenderStyle {
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

    fn blink_interval(&self) -> Option<Duration> {
        let secs = self.config.cursor.blink_interval;
        (secs > 0.0).then(|| Duration::from_secs_f32(secs))
    }

    fn default_layout(&self) -> layout::Layout {
        self.config.layouts.enabled.first().copied().unwrap_or_default()
    }

    fn spawn_pane(
        &mut self,
        metrics: ionterm_render::CellMetrics,
        connection: Option<Connection>,
        cwd: Option<PathBuf>,
    ) -> anyhow::Result<Pane> {
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        let zmodem = zmodem::Shared::new(transfer::download_dir(&self.config.files.download_dir));
        let pane = Pane::spawn(id, &self.config.terminal, connection, cwd, self.theme.palette(), metrics, self.proxy.clone(), zmodem)?;
        if let Some(connection) = &pane.connection {
            self.start_saved_forwards(id, connection);
        }
        Ok(pane)
    }

    /// A pane running a program (an editor, the pager) instead of the shell;
    /// it closes when the program ends.
    fn spawn_command_pane(
        &mut self,
        metrics: ionterm_render::CellMetrics,
        argv: Vec<String>,
        cwd: Option<PathBuf>,
    ) -> anyhow::Result<Pane> {
        let mut terminal = self.config.terminal.clone();
        terminal.command = argv;
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        let zmodem = zmodem::Shared::new(transfer::download_dir(&self.config.files.download_dir));
        Pane::spawn(id, &terminal, None, cwd, self.theme.palette(), metrics, self.proxy.clone(), zmodem)
    }

    /// The command to connect to an ssh config host.
    fn connection_for(&self, alias: &str) -> Connection {
        let hosts = ssh::load_hosts();
        let extras = self.window.as_ref().map_or_else(ssh::load_extras, |w| w.ssh_extras.clone());
        Connection::new(alias, hosts.iter().find(|h| h.alias == alias), extras.get(alias))
    }

    /// The command for `ionterm ssh …`; a destination that is a config host
    /// gets that host's settings too.
    fn connection_from_args(&self, args: &ssh::SshArgs) -> Connection {
        let hosts = ssh::load_hosts();
        let extras = self.window.as_ref().map_or_else(ssh::load_extras, |w| w.ssh_extras.clone());
        let alias = &args.destination;
        Connection::from_args(args, hosts.iter().find(|h| &h.alias == alias), extras.get(alias))
    }

    /// What duplicating a pane runs: the same SSH connection, or a shell in
    /// the same directory.
    fn follow_pane(pane: &Pane) -> (Option<Connection>, Option<PathBuf>) {
        match &pane.connection {
            Some(connection) => (Some(connection.clone()), None),
            None => (None, pane.working_dir()),
        }
    }

    /// Opens the connection picker, centred or under the `⌄` button.
    fn open_palette(&mut self, anchor: Option<(f32, f32)>) {
        let extras = ssh::load_extras();
        let items = ssh::load_hosts()
            .into_iter()
            .map(|host| {
                let extra = extras.get(&host.alias);
                PaletteItem {
                    title: host.alias.clone(),
                    detail: host.target(),
                    group: extra.and_then(|e| e.group.clone()).unwrap_or_default(),
                    icon: extra.and_then(HostExtras::icon_char).unwrap_or(SSH_ICON),
                    color: extra.and_then(|e| e.color).map(|c| c.0),
                    action: PaletteAction::Connect(host.alias),
                }
            })
            .chain([PaletteItem {
                title: "New connection…".into(),
                detail: String::new(),
                group: String::new(),
                icon: '\u{F067}',
                color: None,
                action: PaletteAction::NewConnection,
            }])
            .collect();
        let Some(w) = &mut self.window else { return };
        w.ssh_extras = extras;
        w.palette = Some(Palette::new(items, anchor));
        w.menu = None;
        w.window.request_redraw();
    }

    /// The command palette: every action with its keys, then the hosts.
    fn open_command_palette(&mut self) {
        let extras = ssh::load_extras();
        let commands = crate::commands::catalogue(&self.bindings).into_iter().map(|command| PaletteItem {
            title: command.title.into(),
            detail: command.keys,
            group: command.group.into(),
            icon: command.icon,
            color: None,
            action: PaletteAction::Command(command.action),
        });
        let hosts = ssh::load_hosts().into_iter().map(|host| {
            let extra = extras.get(&host.alias);
            PaletteItem {
                title: format!("SSH: {}", host.alias),
                detail: host.target(),
                group: "SSH hosts".into(),
                icon: extra.and_then(HostExtras::icon_char).unwrap_or(SSH_ICON),
                color: extra.and_then(|e| e.color).map(|c| c.0),
                action: PaletteAction::Connect(host.alias),
            }
        });
        let items = commands.chain(hosts).collect();
        let Some(w) = &mut self.window else { return };
        w.ssh_extras = extras;
        w.palette = Some(Palette::commands(items));
        w.menu = None;
        w.window.request_redraw();
    }

    /// Runs the picker's choice: connect in a new tab, or in a split.
    fn run_palette(&mut self, action: PaletteAction, split: bool) {
        if let Some(w) = &mut self.window {
            w.palette = None;
            w.window.request_redraw();
        }
        match action {
            PaletteAction::Connect(alias) => {
                let connection = self.connection_for(&alias);
                if split {
                    self.split_with(None, Some(connection), None);
                } else {
                    let end = self.window.as_ref().map_or(0, |w| w.tabs.len());
                    self.open_tab(end, Some(connection), None);
                }
            }
            PaletteAction::Command(action) => self.perform(action),
            PaletteAction::MoveTab(to) => {
                if let Some(index) = self.window.as_ref().map(|w| w.active_tab) {
                    self.move_tab(index, to);
                }
            }
            PaletteAction::Forward(text) => {
                let pane = self.window.as_ref().map(|w| w.active_pane().id);
                if let (Some(pane), Ok(spec)) = (pane, crate::forward::Spec::parse(&text)) {
                    self.start_forward(pane, spec);
                }
            }
            PaletteAction::ForwardOther(reverse) => self.ask_forward(reverse),
            PaletteAction::Unforward(reverse, listen) => {
                if let Some(pane) = self.window.as_ref().map(|w| w.active_pane().id) {
                    self.stop_forward(pane, reverse, listen);
                }
            }
            PaletteAction::MovePane(target) => {
                if let Some(id) = self.window.as_ref().map(|w| w.active_pane().id) {
                    self.move_pane(id, target);
                }
            }
            PaletteAction::NewConnection => {
                let editor = Editor::new(&self.theme);
                if let Some(w) = &mut self.window {
                    w.editor = Some(editor);
                }
            }
            PaletteAction::Edit(alias) => {
                let Some(host) = ssh::load_hosts().into_iter().find(|h| h.alias == alias) else { return };
                let extras = ssh::load_extras();
                let editor = Editor::edit(&self.theme, &host, extras.get(&alias), ssh::store::is_managed(&host));
                if let Some(w) = &mut self.window {
                    w.editor = Some(editor);
                }
            }
        }
    }

    /// Carries out what the connection form asked for.
    fn editor_outcome(&mut self, outcome: Outcome) {
        let Some(w) = &mut self.window else { return };
        w.window.request_redraw();
        let Some(editor) = &mut w.editor else { return };
        let previous = editor.previous_alias.clone();
        match outcome {
            Outcome::Cancel => w.editor = None,
            Outcome::Delete => {
                let Some(alias) = previous else { return };
                let result = ssh::store::delete_host(&alias).and_then(|()| ssh::store::delete_extras(&alias));
                match result {
                    Ok(()) => w.editor = None,
                    Err(error) => editor.error = Some(format!("Could not delete: {error}")),
                }
                w.ssh_extras = ssh::load_extras();
            }
            Outcome::Save { connect } => {
                let existing: Vec<String> = ssh::load_hosts().into_iter().map(|h| h.alias).collect();
                let checked = editor
                    .form()
                    .and_then(|form| ssh::store::validate(&form, previous.as_deref(), &existing).map(|()| form))
                    .and_then(|form| editor.forwards().map(|forwards| (form, forwards)));
                let (form, forwards) = match checked {
                    Ok(checked) => checked,
                    Err(message) => {
                        editor.error = Some(message);
                        return;
                    }
                };
                let saved = if editor.read_only_ssh {
                    Ok(())
                } else {
                    ssh::store::save_host(&form, previous.as_deref())
                };
                let result = saved.and_then(|()| {
                    ssh::store::save_extras(&form.alias, previous.as_deref(), editor.color, &editor.icon(), &editor.group(), &forwards)
                });
                if let Err(error) = result {
                    editor.error = Some(format!("Could not save: {error}"));
                    return;
                }
                w.editor = None;
                w.ssh_extras = ssh::load_extras();
                if connect {
                    let connection = self.connection_for(&form.alias);
                    let end = self.window.as_ref().map_or(0, |w| w.tabs.len());
                    self.open_tab(end, Some(connection), None);
                }
            }
        }
    }

    /// A child exited: connections ionterm opened stay open to reconnect if
    /// they ended badly; everything else closes.
    fn child_exited(&mut self, id: PaneId) {
        if self.settings_child_exited(id) {
            return;
        }
        // An editor of a server file: write back what's left.
        self.edit_pane_closed(id);
        if !self.select_window_of_pane(id) {
            return;
        }
        let Some(w) = &mut self.window else { return };
        let Some(pane) = w.tabs.iter_mut().find_map(|t| t.pane_mut(id)) else { return };
        if pane.check_disconnect() {
            w.window.request_redraw();
            return;
        }
        self.remove_pane(id);
    }

    fn reconnect(&mut self) {
        let proxy = self.proxy.clone();
        let Some(w) = &mut self.window else { return };
        let metrics = w.renderer.cell_metrics();
        let tab = w.active_tab;
        let pane = w.tabs[tab].active_pane_mut();
        if let Err(error) = pane.reconnect(&self.config.terminal, metrics, proxy) {
            tracing::warn!("could not reconnect: {error:#}");
        }
        // Its forwards went with the old connection: set them up again.
        let specs = pane.forwards.forget();
        let (id, connection) = (pane.id, pane.connection.clone());
        w.refresh_chips(&self.config);
        if let Some(connection) = connection {
            self.start_forwards_when_connected(id, connection, specs, true);
        }
    }

    fn reload_config(&mut self) {
        let config = Config::load();
        let theme = Theme::load();
        if config == self.config && theme == self.theme {
            return;
        }
        tracing::info!("configuration reloaded");
        let font_changed = config.font != self.config.font;
        let background_changed = (&config.window.background_image, config.window.background_image_opacity)
            != (&self.config.window.background_image, self.config.window.background_image_opacity);
        self.bindings = input::build_bindings(&config.keybindings);
        self.config = config;
        self.theme = theme;
        self.for_each_window(|app| app.apply_config(font_changed, background_changed));
    }

    /// Brings the window being worked on in line with a reloaded config.
    fn apply_config(&mut self, font_changed: bool, background_changed: bool) {
        let style = self.render_style();
        let blink = self.blink_interval();
        let Some(w) = &mut self.window else { return };
        for page in w.tabs.iter_mut().filter_map(|t| t.settings.as_mut()) {
            page.bindings = self.bindings.clone();
        }
        if font_changed {
            w.font_size = self.config.font.size;
        }
        w.renderer.set_style(style);
        w.renderer.set_font(font_options(&self.config.font, w.font_size, w.scale_factor));
        if background_changed {
            match background::load(&self.config.window) {
                Ok(picture) => w.renderer.set_background(picture),
                Err(message) => w.show_toast(message),
            }
        }
        let downloads = transfer::download_dir(&self.config.files.download_dir);
        for pane in w.tabs.iter().flat_map(|t| &t.panes) {
            pane.zmodem.set_download_dir(downloads.clone());
            let mut term = pane.lock();
            term.set_palette(self.theme.palette());
            term.set_scrollback_limit(self.config.terminal.scrollback_lines);
            term.set_advertise_sixel(self.config.terminal.advertise_sixel);
        }
        w.next_blink = blink.map(|d| Instant::now() + d);
        w.blink_on = true;
        w.ssh_extras = ssh::load_extras();
        w.relayout(&self.config, true);
    }

    //#region Clipboard

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
        if self.field_insert(&text) {
            return;
        }
        let Some(w) = &mut self.window else { return };
        let pane = w.active_pane();
        let mode = pane.lock().mode();
        // Without bracketed paste, every line runs as soon as it's pasted.
        let lines = text.trim_end_matches(['\r', '\n']).lines().count();
        if lines > 1 && !mode.contains(TermMode::BRACKETED_PASTE) && self.config.terminal.confirm_multiline_paste {
            let pane = pane.id;
            w.dialog = Some(Dialog::new(
                format!("Paste {lines} lines?"),
                "The program here runs each line as it arrives, so this pastes and runs them all at once.",
                vec![
                    dialog::Button::new("Paste", 'p').danger(),
                    dialog::Button::new("As one line", 'o'),
                    dialog::Button::new("Cancel", 'c'),
                ],
                files::Purpose::Paste { pane, text },
            ));
            if let Some(d) = &mut w.dialog {
                d.default = 2;
            }
            w.window.request_redraw();
            return;
        }
        let id = pane.id;
        w.paste_into(id, &text);
    }

    /// Puts pasted text into the text field that has the keyboard, the way
    /// its owner takes typing. Returns whether something took it.
    fn field_insert(&mut self, text: &str) -> bool {
        let Some(w) = &mut self.window else { return false };
        if let Some(open) = &mut w.dialog {
            open.paste(text);
        } else if let Some(form) = &mut w.editor {
            form.type_text(text);
        } else if let Some(picker) = &mut w.palette {
            picker.type_text(text);
        } else if let Some(rename) = w.rename.as_mut().filter(|_| w.menu.is_none()) {
            rename.field.insert(text);
        } else {
            return self.settings_paste(text) || self.search_paste(text);
        }
        w.window.request_redraw();
        true
    }

    /// Copy, cut and paste in the text field that has the keyboard: ctrl+c,
    /// ctrl+x and ctrl+v, with or without shift, and ctrl+insert and
    /// shift+insert. Returns whether the key was one of those.
    fn field_clipboard_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        let Some(w) = &self.window else { return false };
        let mods = w.modifiers;
        let Some(combo) = KeyCombo::from_event(event, mods) else { return false };
        let (ctrl, shift) = (mods.control_key(), mods.shift_key());
        let command = match combo.key.as_str() {
            _ if mods.alt_key() => return false,
            "c" | "x" | "v" if ctrl => combo.key.as_str(),
            "insert" if ctrl && !shift => "c",
            "insert" if shift && !ctrl => "v",
            _ => return false,
        };
        let Some(field) = w.focused_field() else { return false };
        match command {
            "v" => self.paste(false),
            cut_or_copy => {
                if let Some(text) = field.selected_text().map(String::from) {
                    self.set_clipboard(text, false);
                    if cut_or_copy == "x" {
                        self.field_insert("");
                    }
                }
            }
        }
        true
    }

    fn copy_selection(&mut self, primary: bool) {
        let text = self.window.as_ref().and_then(|w| w.active_pane().selected_text());
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            self.set_clipboard(text, primary);
        }
    }

    fn follow_up(&mut self, follow_up: FollowUp) {
        match follow_up {
            FollowUp::None => {}
            FollowUp::CopyPrimary => self.copy_selection(true),
            FollowUp::PastePrimary => self.paste(cfg!(target_os = "linux")),
            FollowUp::OpenFile(file, cwd) => {
                let path = crate::hints::resolve(&file.path, cwd.as_deref());
                let template = self.config.hints.open_file.clone();
                self.open_file(path, file.line, file.col, cwd, &template);
            }
        }
    }

    //#endregion

    //#region Tabs and panes

    /// A new tab is always a local shell, even next to an SSH tab; only
    /// "Duplicate tab" reconnects to the same host.
    fn new_tab(&mut self) {
        let Some(w) = &self.window else { return };
        let cwd = w.active_pane().working_dir();
        let end = w.tabs.len();
        self.open_tab(end, None, cwd);
    }

    /// Opens a tab at `index`, running a connection or a shell in `cwd`.
    fn open_tab(&mut self, index: usize, connection: Option<Connection>, cwd: Option<PathBuf>) {
        let Some(w) = &self.window else { return };
        let metrics = w.renderer.cell_metrics();
        match self.spawn_pane(metrics, connection, cwd) {
            Ok(pane) => self.insert_tab(index, pane),
            Err(error) => tracing::warn!("could not open a tab: {error:#}"),
        }
    }

    /// Opens a tab running a command (an editor) instead of the shell; it
    /// closes when the command ends.
    fn open_command_tab(&mut self, argv: Vec<String>, cwd: Option<PathBuf>) {
        let Some(w) = &self.window else { return };
        let index = w.active_tab + 1;
        self.open_command_tab_at(index, argv, cwd);
    }

    fn open_command_tab_at(&mut self, index: usize, argv: Vec<String>, cwd: Option<PathBuf>) {
        let Some(w) = &self.window else { return };
        let metrics = w.renderer.cell_metrics();
        match self.spawn_command_pane(metrics, argv, cwd) {
            Ok(pane) => self.insert_tab(index, pane),
            Err(error) => tracing::warn!("could not open a tab: {error:#}"),
        }
    }

    /// Opens the active pane's scrollback in the pager, in a tab after it.
    fn show_scrollback(&mut self) {
        let Some(w) = &mut self.window else { return };
        let rows = w.new_tab_rows(&self.config);
        let pane = w.active_pane();
        let (text, line) = scrollback::export(&pane.lock(), rows);
        let cwd = pane.working_dir();
        let path = match scrollback::write_file(&text, pane.id) {
            Ok(path) => path,
            Err(error) => {
                w.show_toast(format!("Couldn't save the scrollback: {error}"));
                return;
            }
        };
        let argv = scrollback::pager_command(&self.config.terminal.scrollback_pager, &path, line);
        self.open_command_tab(argv, cwd);
        if let Some(w) = &mut self.window {
            w.tabs[w.active_tab].custom_title = Some("Scrollback".into());
        }
    }

    fn insert_tab(&mut self, index: usize, pane: Pane) {
        let layout = self.default_layout();
        let Some(w) = &mut self.window else { return };
        let index = index.min(w.tabs.len());
        w.tabs.insert(index, Tab::new(pane, layout));
        if index <= w.active_tab {
            w.active_tab += 1;
        }
        w.select_tab(index);
        w.relayout(&self.config, false);
    }

    fn menu_command(&mut self, tab: usize, command: MenuCommand) {
        let Some(w) = &mut self.window else { return };
        if tab >= w.tabs.len() {
            return;
        }
        match command {
            MenuCommand::Rename => {
                w.select_tab(tab);
                w.start_rename(tab, &self.config);
            }
            MenuCommand::Duplicate => {
                // A typed ssh is duplicated by running the same command.
                let pane = w.tabs[tab].active_pane();
                let (connection, cwd) = match (&pane.connection, &pane.detected_ssh) {
                    (None, Some(detected)) => (
                        Some(Connection {
                            alias: detected.host().to_string(),
                            argv: detected.argv.clone(),
                            options: Vec::new(),
                            managed: false,
                        }),
                        None,
                    ),
                    _ => Self::follow_pane(pane),
                };
                self.open_tab(tab + 1, connection, cwd);
            }
            MenuCommand::Files => {
                w.select_tab(tab);
                self.open_files(tab);
            }
            MenuCommand::Color(color) => {
                w.tabs[tab].color = color;
                w.window.request_redraw();
            }
            MenuCommand::Forward => {
                w.select_tab(tab);
                self.open_forward_picker();
            }
            MenuCommand::MoveToNewWindow => self.move_tab(tab, None),
            MenuCommand::MoveToWindow => {
                w.select_tab(tab);
                self.open_move_tab_picker();
            }
            MenuCommand::Close => self.close_tab(tab),
            MenuCommand::CloseOthers => {
                let count = w.tabs.len();
                for index in (0..count).rev().filter(|&i| i != tab) {
                    self.close_tab(index);
                }
            }
        }
    }

    /// Splits open a local shell too, like new tabs.
    fn split(&mut self, direction: Option<Direction>) {
        let Some(w) = &self.window else { return };
        let cwd = w.active_pane().working_dir();
        self.split_with(direction, None, cwd);
    }

    fn split_with(&mut self, direction: Option<Direction>, connection: Option<Connection>, cwd: Option<PathBuf>) {
        let Some(w) = &self.window else { return };
        let metrics = w.renderer.cell_metrics();
        let pane = self.spawn_pane(metrics, connection, cwd);
        self.add_split(direction, pane);
    }

    /// Splits the active pane with one running `command` (or the shell).
    fn split_running(&mut self, direction: Option<Direction>, command: Option<Vec<String>>, cwd: Option<PathBuf>) {
        let Some(command) = command else {
            self.split_with(direction, None, cwd);
            return;
        };
        let Some(w) = &self.window else { return };
        let metrics = w.renderer.cell_metrics();
        let pane = self.spawn_command_pane(metrics, command, cwd);
        self.add_split(direction, pane);
    }

    /// Puts a new pane beside the active one, along the longer side unless
    /// told, and focuses it.
    fn add_split(&mut self, direction: Option<Direction>, pane: anyhow::Result<Pane>) {
        let Some(w) = &self.window else { return };
        let active = w.active_pane();
        let direction = direction.unwrap_or(if active.area.width >= active.area.height {
            Direction::Horizontal
        } else {
            Direction::Vertical
        });
        match pane {
            Ok(pane) => {
                let Some(w) = &mut self.window else { return };
                let new_id = pane.id;
                let tab = &mut w.tabs[w.active_tab];
                let old = tab.active;
                tab.add(pane, direction);
                tab.active = old;
                w.set_active_pane(new_id);
                w.relayout(&self.config, false);
            }
            Err(error) => tracing::warn!("could not split: {error:#}"),
        }
    }

    /// Removes a pane wherever it is, closing its tab when it was the last
    /// one, and the window when that was the last tab.
    fn remove_pane(&mut self, id: PaneId) {
        let Some(w) = &mut self.window else { return };
        let Some(tab_index) = w.tabs.iter().position(|t| t.pane(id).is_some()) else {
            return;
        };
        let was_active = tab_index == w.active_tab && w.tabs[tab_index].active == id;
        drop(w.tabs[tab_index].remove(id));
        // The panel browses through this pane's connection.
        if w.tabs[tab_index].files.as_ref().is_some_and(|p| p.pane == id) {
            w.tabs[tab_index].files = None;
        }
        if w.capture == Some(id) {
            w.capture = None;
        }
        if w.tabs[tab_index].panes.is_empty() {
            self.close_tab(tab_index);
            return;
        }
        if was_active && w.focused {
            send_focus(w.active_pane(), true);
        }
        w.update_title();
        w.relayout(&self.config, false);
    }

    fn close_tab(&mut self, index: usize) {
        let Some(w) = &mut self.window else { return };
        if index >= w.tabs.len() {
            return;
        }
        // Dropping the tab drops its panes, which ends their shells.
        drop(w.tabs.remove(index));
        w.rename = None;
        w.menu = None;
        w.tab_drag = None;
        w.tab_hover = Hit::None;
        if w.tabs.is_empty() {
            // Closing every tab of the last window yourself ends the session.
            if self.parked.is_empty() && self.pending_windows.is_empty() {
                crate::session::remove_ours();
            }
            // Events still queued (the key's release) must not find a window
            // without tabs.
            self.close_window();
            return;
        }
        if w.active_tab > index || w.active_tab >= w.tabs.len() {
            w.active_tab = w.active_tab.saturating_sub(1);
        }
        w.capture = None;
        if w.focused {
            send_focus(w.active_pane(), true);
        }
        w.update_title();
        w.relayout(&self.config, false);
    }

    //#endregion

    fn perform(&mut self, action: Action) {
        match action {
            Action::Copy => self.copy_selection(false),
            Action::Paste => self.paste(false),
            Action::PasteSelection => self.paste(cfg!(target_os = "linux")),
            Action::ReloadConfig => self.reload_config(),
            Action::NewTab => self.new_tab(),
            Action::NewWindow => self.new_window(),
            Action::MoveTabToNewWindow => {
                if let Some(index) = self.window.as_ref().map(|w| w.active_tab) {
                    self.move_tab(index, None);
                }
            }
            Action::MoveTabToWindow => self.open_move_tab_picker(),
            Action::ForwardPort => self.open_forward_picker(),
            Action::MovePaneToNewTab | Action::MovePaneToNewWindow => {
                let target = if action == Action::MovePaneToNewTab { PaneTarget::NewTab } else { PaneTarget::NewWindow };
                if let Some(id) = self.window.as_ref().map(|w| w.active_pane().id) {
                    self.move_pane(id, target);
                }
            }
            Action::MovePane => self.open_move_pane_picker(),
            Action::CloseTab => {
                if let Some(index) = self.window.as_ref().map(|w| w.active_tab) {
                    self.close_tab(index);
                }
            }
            Action::SshConnect => self.open_palette(None),
            Action::ToggleFiles => self.toggle_files(),
            Action::PreviousPrompt => self.jump_to_prompt(false),
            Action::NextPrompt => self.jump_to_prompt(true),
            Action::CopyLastOutput => self.copy_last_output(),
            Action::CopyCommand => self.current_block_action(Some(crate::blocks::BlockAction::CopyCommand)),
            Action::CopyCommandMarkdown => self.current_block_action(Some(crate::blocks::BlockAction::CopyMarkdown)),
            Action::RerunCommand => self.current_block_action(Some(crate::blocks::BlockAction::Rerun)),
            Action::EditCommandOutput => self.current_block_action(Some(crate::blocks::BlockAction::OpenInEditor)),
            Action::PageCommandOutput => self.current_block_action(Some(crate::blocks::BlockAction::OpenInPager)),
            Action::SaveCommandOutput => self.current_block_action(Some(crate::blocks::BlockAction::SaveOutput)),
            Action::FoldOutput => self.current_block_action(Some(crate::blocks::BlockAction::Fold)),
            Action::CommandMenu => self.current_block_action(None),
            Action::Hints => self.toggle_hints(),
            Action::OpenSettings => self.open_settings(),
            Action::Search => self.toggle_search(),
            Action::ShowScrollback => self.show_scrollback(),
            Action::ToggleBroadcast => {
                if let Some(w) = &mut self.window {
                    w.toggle_broadcast();
                }
            }
            Action::ClearScrollback => {
                let Some(w) = &mut self.window else { return };
                let pane = w.active_pane();
                pane.lock().clear_history();
                let id = pane.id;
                w.search_output(id);
                w.window.request_redraw();
            }
            Action::CopyCwd => {
                let Some(w) = &self.window else { return };
                let pane = w.active_pane();
                let dir = match &pane.remote_cwd {
                    Some(remote) if pane.connection.is_some() || pane.detected_ssh.is_some() => Some(remote.clone()),
                    _ => pane.working_dir().map(|d| d.display().to_string()),
                };
                match dir {
                    Some(dir) => {
                        if let Some(w) = &mut self.window {
                            w.show_toast(format!("Copied {dir}"));
                        }
                        self.set_clipboard(dir, false);
                    }
                    None => {
                        if let Some(w) = &mut self.window {
                            w.show_toast("Don't know this pane's directory");
                        }
                    }
                }
            }
            Action::OpenCwd => {
                let Some(w) = &mut self.window else { return };
                let pane = w.active_pane();
                if pane.connection.is_some() || pane.detected_ssh.is_some() {
                    w.show_toast("That directory is on the server: ctrl+shift+o browses it");
                } else if let Some(dir) = pane.working_dir() {
                    crate::files::reveal(&dir);
                }
            }
            Action::CommandPalette => self.open_command_palette(),
            Action::NewSplit => self.split(None),
            Action::SplitRight => self.split(Some(Direction::Horizontal)),
            Action::SplitDown => self.split(Some(Direction::Vertical)),
            Action::ClosePane => {
                if let Some(id) = self.window.as_ref().map(|w| w.active_pane().id) {
                    self.remove_pane(id);
                }
            }
            Action::None => {}
            _ => self.perform_in_window(action),
        }
    }

    /// Actions that only rearrange or scroll what the window already has.
    fn perform_in_window(&mut self, action: Action) {
        let font = self.config.font.clone();
        let enabled_layouts = self.config.layouts.enabled.clone();
        let Some(w) = &mut self.window else { return };
        let tab_count = w.tabs.len();
        let mut relayout = false;
        match action {
            Action::NextTab => w.select_tab((w.active_tab + 1) % tab_count),
            Action::PreviousTab => w.select_tab((w.active_tab + tab_count - 1) % tab_count),
            Action::GotoTab(n) if n <= tab_count => w.select_tab(n - 1),
            Action::RenameTab => {
                let tab = w.active_tab;
                w.start_rename(tab, &self.config);
            }
            Action::MoveTabForward | Action::MoveTabBackward => {
                let offset: isize = if action == Action::MoveTabForward { 1 } else { -1 };
                let other = (w.active_tab as isize + offset).rem_euclid(tab_count as isize) as usize;
                w.tabs.swap(w.active_tab, other);
                w.active_tab = other;
            }
            Action::NextPane => {
                let id = w.tabs[w.active_tab].cycle(1);
                w.set_active_pane(id);
            }
            Action::PreviousPane => {
                let id = w.tabs[w.active_tab].cycle(-1);
                w.set_active_pane(id);
            }
            Action::MovePaneForward => {
                w.tabs[w.active_tab].move_active(1);
                relayout = true;
            }
            Action::MovePaneBackward => {
                w.tabs[w.active_tab].move_active(-1);
                relayout = true;
            }
            Action::FocusLeft | Action::FocusRight | Action::FocusUp | Action::FocusDown => {
                let (dx, dy) = match action {
                    Action::FocusLeft => (-1, 0),
                    Action::FocusRight => (1, 0),
                    Action::FocusUp => (0, -1),
                    _ => (0, 1),
                };
                let tab = &w.tabs[w.active_tab];
                let rects: Vec<(PaneId, Rect)> = tab.panes.iter().map(|p| (p.id, p.area)).collect();
                if let Some(id) = layout::neighbor(&rects, tab.active, dx, dy) {
                    w.set_active_pane(id);
                }
            }
            Action::ResizeWider | Action::ResizeNarrower | Action::ResizeTaller | Action::ResizeShorter => {
                let (direction, delta) = match action {
                    Action::ResizeWider => (Direction::Horizontal, RESIZE_STEP),
                    Action::ResizeNarrower => (Direction::Horizontal, -RESIZE_STEP),
                    Action::ResizeTaller => (Direction::Vertical, RESIZE_STEP),
                    _ => (Direction::Vertical, -RESIZE_STEP),
                };
                relayout = w.tabs[w.active_tab].resize_active(direction, delta);
            }
            Action::NextLayout => {
                let tab = &mut w.tabs[w.active_tab];
                let current = enabled_layouts.iter().position(|&l| l == tab.layout);
                let next = current.map_or(0, |i| (i + 1) % enabled_layouts.len());
                if let Some(&layout) = enabled_layouts.get(next) {
                    tab.layout = layout;
                    tab.zoomed = false;
                    relayout = true;
                }
            }
            Action::ToggleZoom => {
                let tab = &mut w.tabs[w.active_tab];
                tab.zoomed = !tab.zoomed;
                relayout = true;
            }
            Action::IncreaseFontSize | Action::DecreaseFontSize | Action::ResetFontSize => {
                w.font_size = match action {
                    Action::IncreaseFontSize => w.font_size + 1.0,
                    Action::DecreaseFontSize => (w.font_size - 1.0).max(MIN_FONT_SIZE),
                    _ => font.size,
                };
                w.renderer.set_font(font_options(&font, w.font_size, w.scale_factor));
                w.relayout(&self.config, true);
            }
            _ => {
                let tab = w.active_tab;
                w.tabs[tab].active_pane_mut().scrolled_at = Some(Instant::now());
                let pane = w.active_pane();
                let page = pane.grid.1 as isize;
                let mut term = pane.lock();
                match action {
                    Action::ScrollLineUp => term.scroll_display(1),
                    Action::ScrollLineDown => term.scroll_display(-1),
                    Action::ScrollPageUp => term.scroll_display(page),
                    Action::ScrollPageDown => term.scroll_display(-page),
                    Action::ScrollHome => term.scroll_display(isize::MAX / 2),
                    Action::ScrollEnd => term.scroll_to_bottom(),
                    _ => {}
                }
            }
        }
        if relayout {
            w.relayout(&self.config, false);
        }
        w.window.request_redraw();
    }

    fn handle_wakeup(&mut self, id: PaneId) {
        let Some(w) = &mut self.window else { return };
        let Some(tab_index) = w.tabs.iter().position(|t| t.pane(id).is_some()) else {
            return;
        };
        let shown = tab_index == w.active_tab && w.tabs[tab_index].is_visible(id);
        let is_active = shown && w.tabs[tab_index].active == id;
        let bar_shown = w.tab_bar.is_some();
        let focused = w.focused;
        let pane = w.tabs[tab_index].pane_mut(id).expect("found above");
        pane.wakeup_pending.store(false, Ordering::Release);
        let (events, sync_started) = {
            let mut term = pane.lock();
            (term.take_events(), term.sync_started())
        };

        let mut redraw = bar_shown && pane.refresh_process(Instant::now());
        if shown {
            match sync_started {
                Some(start) if start.elapsed() < SYNC_TIMEOUT => pane.sync_deadline = Some(start + SYNC_TIMEOUT),
                _ => {
                    pane.sync_deadline = None;
                    redraw = true;
                }
            }
        }
        let mut clipboard = None;
        let mut title_changed = false;
        let mut bell = false;
        let mut remote_cwd = false;
        let mut file_requests = Vec::new();
        let mut progress_changed = false;
        let mut finished = None;
        let mut notifications = Vec::new();
        for event in events {
            match event {
                TermEvent::Title(title) => {
                    pane.title = title;
                    title_changed = true;
                }
                TermEvent::Bell => bell = true,
                TermEvent::ClipboardStore(text) => clipboard = Some(text),
                TermEvent::Cwd(url) => remote_cwd |= pane.report_cwd(&url, &self.local_host),
                TermEvent::SyncEnd => redraw |= shown,
                TermEvent::FileRequest(request) => file_requests.push(request),
                TermEvent::Progress(progress) => progress_changed |= pane.report_progress(progress),
                // Back at the prompt: whatever was in progress is over.
                TermEvent::Prompt => progress_changed |= pane.report_progress(None),
                TermEvent::CommandStarted(line) => pane.command = Some((Instant::now(), line)),
                TermEvent::CommandFinished(status) => {
                    if let Some((started, line)) = pane.command.take() {
                        finished = Some((started.elapsed(), line, status));
                    }
                }
                TermEvent::Notify { title, body } => notifications.push((title, body)),
            }
        }
        progress_changed |= pane.guess_progress();
        if title_changed {
            if is_active {
                w.update_title();
            }
            redraw |= bar_shown;
        }
        redraw |= progress_changed && bar_shown;
        if bell && !focused {
            w.window.request_user_attention(Some(winit::window::UserAttentionType::Informational));
        }
        // Marks on tabs you're not looking at.
        let elsewhere = tab_index != w.active_tab;
        let long_after = self.config.notifications.command_done_after;
        let long = finished.as_ref().filter(|f| long_after > 0.0 && f.0.as_secs_f32() >= long_after);
        if elsewhere {
            let tab = &mut w.tabs[tab_index];
            let before = tab.activity;
            tab.activity = match (long, bell, before) {
                (_, true, _) => Activity::Bell,
                (Some(f), _, _) => Activity::Done(f.2.is_none_or(|s| s == 0)),
                (None, false, Activity::None) => Activity::Output,
                (None, false, other) => other,
            };
            redraw |= bar_shown && tab.activity != before;
        }
        // A long command ended while you were elsewhere: say so.
        if let Some((took, line, status)) = long
            && (elsewhere || !focused)
        {
            let what = line.as_deref().map(str::trim).filter(|l| !l.is_empty()).unwrap_or("Command");
            let title = match status {
                Some(0) | None => format!("✓ {}", crate::tabbar::truncate(what, 60)),
                Some(code) => format!("✗ {} (exit {code})", crate::tabbar::truncate(what, 60)),
            };
            notifications::send(&title, &format!("Finished after {}", notifications::duration(*took)));
        }
        if self.config.notifications.from_programs {
            for (title, body) in notifications {
                notifications::send(&title, &body);
            }
        }
        if redraw {
            w.window.request_redraw();
        }
        if let Some(text) = clipboard {
            self.set_clipboard(text, false);
        }
        if let Some(w) = &mut self.window {
            w.search_output(id);
        }
        // Shells report after every command, not only after a `cd`, so a
        // panel browsed elsewhere comes back once the terminal is used.
        if remote_cwd {
            self.follow_cwd(tab_index, id);
        }
        for request in file_requests {
            self.file_request(id, request);
        }
    }

    //#region Mouse

    fn handle_mouse_button(&mut self, button: winit::event::MouseButton, state: ElementState) {
        let follow_up = self.mouse_button(button, state);
        self.follow_up(follow_up);
    }

    fn mouse_button(&mut self, button: winit::event::MouseButton, state: ElementState) -> FollowUp {
        let button = match button {
            winit::event::MouseButton::Left => MouseButton::Left,
            winit::event::MouseButton::Middle => MouseButton::Middle,
            winit::event::MouseButton::Right => MouseButton::Right,
            _ => return FollowUp::None,
        };
        // An open block menu takes the click.
        let pressed = state == ElementState::Pressed;
        if self.window.as_ref().is_some_and(|w| w.block_menu.is_some()) && self.blocks_click(button, pressed) {
            return FollowUp::None;
        }
        // A click dismisses hints.
        if state == ElementState::Pressed
            && let Some(w) = &mut self.window
            && w.hints.take().is_some()
        {
            w.window.request_redraw();
            return FollowUp::None;
        }
        let pressed = state == ElementState::Pressed;
        if let Some(action) = self.tab_bar_click(button, pressed) {
            match action {
                Some(BarAction::NewTab) => self.new_tab(),
                Some(BarAction::Settings) => self.open_settings(),
                Some(BarAction::Run(action, split)) => self.run_palette(action, split),
                Some(BarAction::Form(outcome)) => self.editor_outcome(outcome),
                Some(BarAction::Palette(anchor)) => {
                    // Line the picker's right edge up with the button.
                    let width = self.window.as_ref().map_or(0.0, |w| w.renderer.cell_metrics().width * 64.0);
                    self.open_palette(anchor.map(|(right, bottom)| (right - width, bottom)));
                }
                Some(BarAction::Close(index)) => self.close_tab(index),
                Some(BarAction::Menu(command, tab)) => self.menu_command(tab, command),
                Some(BarAction::Dialog(answer)) => self.dialog_answer(answer),
                Some(BarAction::DetachTab(index)) => self.start_tab_drop(index),
                Some(BarAction::Chip(index, stop)) => self.chip_click(index, stop),
                None => {}
            }
            return FollowUp::None;
        }
        if self.blocks_click(button, pressed) {
            return FollowUp::None;
        }
        if self.files_click(button, pressed) {
            return FollowUp::None;
        }
        if pressed
            && button == MouseButton::Left
            && let Some(w) = &self.window
        {
            let (x, y) = (w.pointer.x as f32, w.pointer.y as f32);
            if self.search_click(x, y) {
                return FollowUp::None;
            }
        }
        // The settings page takes every click on it.
        if let Some(w) = &self.window
            && w.settings_page().is_some()
        {
            let (x, y) = (w.pointer.x as f32, w.pointer.y as f32);
            if pressed && button == MouseButton::Left {
                self.settings_click(x, y);
            }
            return FollowUp::None;
        }
        let Some(w) = &mut self.window else { return FollowUp::None };
        let (x, y) = (w.pointer.x, w.pointer.y);

        // A press on the line between split panes drags it.
        if button == MouseButton::Left {
            if !pressed && w.split_drag.take().is_some() {
                return FollowUp::None;
            }
            if pressed && let Some(border) = w.border_at(x as f32, y as f32) {
                w.split_drag = Some(border);
                return FollowUp::None;
            }
        }

        let target = if pressed { w.pane_at(x, y) } else { w.capture.take() };
        let Some(id) = target else { return FollowUp::None };
        if pressed {
            w.capture = Some(id);
            w.set_active_pane(id);
        }
        let m = w.renderer.cell_metrics();
        let modifiers = w.modifiers;
        let Some(pane) = w.tabs[w.active_tab].pane_mut(id) else { return FollowUp::None };
        let (col, row) = pane.cell_at(x, y, m);
        let mode = pane.lock().mode();
        // Shift bypasses mouse reporting so text can still be selected.
        let grabbed = mode.intersects(TermMode::MOUSE_REPORTING) && !modifiers.shift_key();

        if !grabbed
            && pressed
            && button == MouseButton::Left
            && modifiers.control_key()
            && let Some(link) = &pane.hovered
        {
            if let Some(file) = &link.file {
                return FollowUp::OpenFile(file.clone(), pane.working_dir());
            }
            links::open(&link.uri);
            return FollowUp::None;
        }
        if grabbed {
            let action = if pressed { MouseAction::Press } else { MouseAction::Release };
            if let Some(bytes) = input::encode_mouse(action, Some(button), col, row, modifiers, mode) {
                pane.write(&bytes);
            }
            pane.held_button = pressed.then_some(button);
            return FollowUp::None;
        }
        pane.held_button = pressed.then_some(button);

        // A plain click on the command line moves the shell's cursor there,
        // when the shell takes clicks (OSC 133 click_events): one SGR click.
        let plain = !modifiers.shift_key() && !modifiers.control_key() && !modifiers.alt_key();
        if pressed && button == MouseButton::Left && plain && pane.lock().click_goes_to_shell(row) {
            let (x, y) = (col + 1, row + 1);
            pane.write(format!("\x1b[<0;{x};{y}M\x1b[<0;{x};{y}m").as_bytes());
            pane.selection = None;
            w.window.request_redraw();
            return FollowUp::None;
        }

        let follow_up = match (button, pressed) {
            // ctrl+shift+right-click selects that command's output.
            (MouseButton::Right, true) if modifiers.control_key() && modifiers.shift_key() => {
                if integration::select_command_output(pane, row) { FollowUp::CopyPrimary } else { FollowUp::None }
            }
            (MouseButton::Left, true) => {
                let absolute = (pane.lock().viewport_to_absolute(row), col);
                let count = match pane.last_click {
                    Some((at, pos, n)) if pos == absolute && at.elapsed() < MULTI_CLICK => n % 3 + 1,
                    _ => 1,
                };
                pane.last_click = Some((Instant::now(), absolute, count));
                pane.selecting = true;
                pane.selection = match count {
                    1 => None,
                    n => Some(SelectionState {
                        anchor: absolute,
                        head: absolute,
                        kind: if n == 2 { SelectionKind::Word } else { SelectionKind::Line },
                    }),
                };
                FollowUp::None
            }
            (MouseButton::Left, false) => {
                pane.selecting = false;
                if pane.selection.is_some() { FollowUp::CopyPrimary } else { FollowUp::None }
            }
            (MouseButton::Middle, true) => FollowUp::PastePrimary,
            _ => FollowUp::None,
        };
        w.window.request_redraw();
        follow_up
    }

    /// Handles clicks on the menu and the tab bar. `None` = not ours, pass
    /// it on to the panes; `Some(action)` = consumed, with work to run after.
    fn tab_bar_click(&mut self, button: MouseButton, pressed: bool) -> Option<Option<BarAction>> {
        let w = self.window.as_mut()?;
        let (x, y) = (w.pointer.x, w.pointer.y);
        let m = w.renderer.cell_metrics();
        let scale = w.scale_factor as f32;

        if !pressed
            && button == MouseButton::Left
            && let Some(drag) = w.tab_drag.take()
        {
            w.window.request_redraw();
            return Some((drag.moved && drag.outside).then_some(BarAction::DetachTab(drag.index)));
        }
        // The dialog is modal: it takes every click while open.
        let window_rect = w.window_rect();
        if let Some(open) = &mut w.dialog {
            if !pressed || button != MouseButton::Left {
                return Some(None);
            }
            let layout = dialog::layout(open, m, scale, window_rect);
            w.window.request_redraw();
            let target = dialog::hit(&layout, x as f32, y as f32);
            if target == Some(Some(dialog::Target::Input)) {
                open.place_caret(&layout, x as f32, m, scale, w.modifiers.shift_key());
            }
            return Some(open.click(target).map(BarAction::Dialog));
        }
        // The form is modal: it takes every click while open.
        if let Some(form) = &mut w.editor {
            if !pressed || button != MouseButton::Left {
                return Some(None);
            }
            let layout = editor::layout(form, m, scale, window_rect);
            w.window.request_redraw();
            let target = editor::hit(&layout, x as f32, y as f32).flatten();
            let outcome = target.and_then(|target| form.click(target));
            if matches!(target, Some(editor::Target::Field(_))) {
                form.place_caret(&layout, x as f32, m, scale, w.modifiers.shift_key());
            }
            return Some(outcome.map(BarAction::Form));
        }
        // The picker takes clicks while open: a row runs it, outside closes it.
        if let Some(picker) = &w.palette {
            if !pressed {
                return Some(None);
            }
            let layout = palette::layout(picker, m, scale, w.window_rect());
            w.window.request_redraw();
            return Some(match palette::hit(&layout, x as f32, y as f32) {
                None => {
                    w.palette = None;
                    None
                }
                Some(Some(position)) if button == MouseButton::Left => {
                    let item = picker.matches()[position];
                    let action = picker.items[item].action.clone();
                    let split = w.modifiers.control_key();
                    Some(BarAction::Run(action, split))
                }
                Some(_) => None,
            });
        }
        // An open menu takes the next click, wherever it lands.
        if let Some(open) = w.menu.take() {
            if !pressed {
                w.menu = Some(open);
                return Some(None);
            }
            w.window.request_redraw();
            let layout = menu::layout(&open, &self.theme, m, scale, w.window_rect());
            return Some(match menu::hit(&layout, x as f32, y as f32) {
                Some(Some(command)) if button == MouseButton::Left => Some(BarAction::Menu(command, open.tab)),
                _ => None,
            });
        }

        let infos = w.tab_infos(&self.config);
        let layout = w.bar_layout(&self.config, &infos);
        let hit = layout
            .as_ref()
            .map_or(Hit::None, |layout| tabbar::hit(layout, x as f32, y as f32));
        // Clicking anywhere but the tab being renamed finishes the rename.
        if pressed && w.rename.as_ref().is_some_and(|r| hit != Hit::Tab(r.tab)) {
            w.commit_rename();
        }
        if hit == Hit::None {
            return None;
        }
        if !pressed {
            return Some(None);
        }
        let action = match (button, hit) {
            (MouseButton::Left, Hit::Tab(i)) => {
                let now = Instant::now();
                let double = w
                    .last_tab_click
                    .is_some_and(|(at, tab)| tab == i && now.duration_since(at) < MULTI_CLICK);
                w.last_tab_click = Some((now, i));
                w.select_tab(i);
                if double {
                    w.start_rename(i, &self.config);
                } else {
                    // Grab the tab where it is drawn, which may be mid-glide.
                    let drawn_x = w
                        .tab_positions
                        .get(&w.tabs[i].id)
                        .copied()
                        .or(layout.as_ref().map(|l| l.tabs[i].rect.x))
                        .unwrap_or(0.0);
                    w.tab_drag = Some(TabDrag {
                        index: i,
                        start_x: x,
                        grab_dx: x - f64::from(drawn_x),
                        moved: false,
                        outside: false,
                    });
                }
                None
            }
            (MouseButton::Left, Hit::Close(i)) | (MouseButton::Middle, Hit::Tab(i) | Hit::Close(i)) => {
                Some(BarAction::Close(i))
            }
            (MouseButton::Right, Hit::Tab(i) | Hit::Close(i)) => {
                let bar_bottom = w.tab_bar.map_or(0.0, |b| b.y + b.height);
                let below = w.tab_bar.is_some_and(|b| b.y == 0.0);
                let ssh = w.tabs[i].active_pane().connection.as_ref().is_some_and(|c| c.managed);
                w.menu = Some(Menu {
                    tab: i,
                    x: x as f32,
                    y: if below { bar_bottom } else { y as f32 },
                    hover: None,
                    ssh,
                });
                w.window.request_redraw();
                None
            }
            (MouseButton::Left, Hit::Chip(i)) => Some(BarAction::Chip(i, false)),
            (MouseButton::Right | MouseButton::Middle, Hit::Chip(i)) => Some(BarAction::Chip(i, true)),
            (MouseButton::Left, Hit::NewTab) => Some(BarAction::NewTab),
            (MouseButton::Left, Hit::Settings) => Some(BarAction::Settings),
            (MouseButton::Left, Hit::Dropdown) => {
                let anchor = layout.as_ref().and_then(|l| l.dropdown).map(|r| (r.x + r.width, r.y + r.height));
                Some(BarAction::Palette(anchor))
            }
            _ => None,
        };
        Some(action)
    }

    fn handle_cursor_moved(&mut self, position: PhysicalPosition<f64>) {
        let Some(w) = &mut self.window else { return };
        w.pointer = position;
        let (x, y) = (position.x, position.y);
        let m = w.renderer.cell_metrics();
        let scale = w.scale_factor as f32;

        if let Some(border) = &w.split_drag {
            let pos = match border.direction {
                Direction::Horizontal => x as f32,
                Direction::Vertical => y as f32,
            };
            let (path, ratio) = (border.path.clone(), border.ratio_at(pos));
            let tab = w.active_tab;
            w.tabs[tab].set_split_ratio(&path, ratio);
            w.relayout(&self.config, false);
            w.window.request_redraw();
            return;
        }
        if w.capture.is_none()
            && w.dialog.is_none()
            && w.editor.is_none()
            && let Some(border) = w.border_at(x as f32, y as f32)
        {
            w.set_cursor_icon(match border.direction {
                Direction::Horizontal => CursorIcon::ColResize,
                Direction::Vertical => CursorIcon::RowResize,
            });
            return;
        }

        if let Some(open) = &w.dialog {
            let layout = dialog::layout(open, m, scale, w.window_rect());
            let hover = dialog::hit(&layout, x as f32, y as f32).flatten();
            let icon = match hover {
                Some(dialog::Target::Input) => CursorIcon::Text,
                Some(_) => CursorIcon::Pointer,
                None => CursorIcon::Default,
            };
            if let Some(open) = &mut w.dialog
                && open.hover != hover
            {
                open.hover = hover;
                w.window.request_redraw();
            }
            w.set_cursor_icon(icon);
            return;
        }
        if let Some(form) = &w.editor {
            let layout = editor::layout(form, m, scale, w.window_rect());
            let hover = editor::hit(&layout, x as f32, y as f32).flatten();
            let icon = match hover {
                Some(editor::Target::Field(_)) => CursorIcon::Text,
                Some(_) => CursorIcon::Pointer,
                None => CursorIcon::Default,
            };
            if let Some(form) = &mut w.editor
                && form.hover != hover
            {
                form.hover = hover;
                w.window.request_redraw();
            }
            w.set_cursor_icon(icon);
            return;
        }
        if let Some(picker) = &w.palette {
            let layout = palette::layout(picker, m, scale, w.window_rect());
            let hover = palette::hit(&layout, x as f32, y as f32).flatten();
            if let Some(picker) = &mut w.palette
                && picker.hover != hover
            {
                picker.hover = hover;
                w.window.request_redraw();
            }
            w.set_cursor_icon(if hover.is_some() { CursorIcon::Pointer } else { CursorIcon::Default });
            return;
        }
        if let Some(open) = &w.menu {
            let layout = menu::layout(open, &self.theme, m, scale, w.window_rect());
            let hover = menu::hit(&layout, x as f32, y as f32).flatten();
            let icon = if hover.is_some() { CursorIcon::Pointer } else { CursorIcon::Default };
            if let Some(open) = &mut w.menu
                && open.hover != hover
            {
                open.hover = hover;
                w.window.request_redraw();
            }
            w.set_cursor_icon(icon);
            return;
        }
        if w.tab_drag.is_some() {
            // Far enough from the bar (or past the sides), letting go moves
            // the tab out of the window.
            let away = TAB_DETACH_DISTANCE * f64::from(scale);
            let size = w.window.inner_size();
            let bar = w.tab_bar.unwrap_or_default();
            let (top, bottom) = (f64::from(bar.y), f64::from(bar.y + bar.height));
            let outside = x < -away || x > f64::from(size.width) + away || y < top - away || y > bottom + away;
            let alone = w.tabs.len() == 1;
            let Some(drag) = &mut w.tab_drag else { return };
            if !drag.moved && ((x - drag.start_x).abs() > TAB_DRAG_THRESHOLD * f64::from(scale) || outside) {
                drag.moved = true;
            }
            let left_now = drag.moved && outside && !drag.outside;
            if drag.moved {
                drag.outside = outside;
            }
            if left_now {
                w.show_toast(if alone {
                    "Let go over another ionterm window to move the tab there"
                } else {
                    "Let go to move the tab to a new window, or over another ionterm window"
                });
            }
            if let Some(drag) = &w.tab_drag
                && drag.moved
            {
                let (index, grab_dx) = (drag.index, drag.grab_dx);
                let infos = w.tab_infos(&self.config);
                if let Some(layout) = w.bar_layout(&self.config, &infos) {
                    // Swap once the dragged tab's centre passes a neighbour's.
                    let width = layout.tabs[index].rect.width;
                    let center = (x - grab_dx) as f32 + width / 2.0;
                    let target = tabbar::slot_at_x(&layout, center);
                    if target != index {
                        let tab = w.tabs.remove(index);
                        w.tabs.insert(target, tab);
                        w.active_tab = target;
                        if let Some(drag) = &mut w.tab_drag {
                            drag.index = target;
                        }
                    }
                }
                // The tab follows the pointer every frame while dragged.
                w.window.request_redraw();
            }
            return;
        }
        if w.blocks_motion(x as f32, y as f32, &self.config) {
            return;
        }
        if w.settings_hover(x as f32, y as f32) || w.search_hover(x as f32, y as f32) {
            return;
        }
        if w.capture.is_none() && w.files_hover(x as f32, y as f32) {
            // Leaving the panes: no pane cell is under the mouse any more.
            w.pointer_cell = None;
            for pane in &mut w.tabs[w.active_tab].panes {
                if pane.update_hover(None, false) {
                    w.window.request_redraw();
                }
            }
            return;
        }

        let infos = w.tab_infos(&self.config);
        let hit = w
            .bar_layout(&self.config, &infos)
            .map_or(Hit::None, |layout| tabbar::hit(&layout, x as f32, y as f32));
        if hit != w.tab_hover {
            w.tab_hover = hit;
            w.window.request_redraw();
            if let Hit::Chip(index) = hit {
                self.chip_hint(index);
            }
        }
        let Some(w) = &mut self.window else { return };
        let over_bar = hit != Hit::None;
        let under = if over_bar { None } else { w.pane_at(x, y) };
        let target = w.capture.or(under);

        let cell = target.and_then(|id| w.tabs[w.active_tab].pane(id)).map(|p| (p.id, p.cell_at(x, y, m)));
        if cell == w.pointer_cell {
            return;
        }
        // Hover belongs to the pane under the pointer; clear it elsewhere.
        let mut redraw = false;
        let ctrl = w.modifiers.control_key();
        for pane in &mut w.tabs[w.active_tab].panes {
            let hover_cell = (under == Some(pane.id)).then(|| pane.cell_at(x, y, m));
            redraw |= pane.update_hover(hover_cell, ctrl);
        }
        let on_link = under
            .and_then(|id| w.tabs[w.active_tab].pane(id))
            .is_some_and(|p| p.hovered.is_some());
        w.set_cursor_icon(if matches!(hit, Hit::Close(_) | Hit::NewTab | Hit::Dropdown | Hit::Settings | Hit::Chip(_)) {
            CursorIcon::Pointer
        } else if over_bar {
            CursorIcon::Default
        } else if on_link {
            CursorIcon::Pointer
        } else {
            CursorIcon::Text
        });
        let chip_before = w.hovered_command().map(|c| (c.0, c.1));
        w.pointer_cell = cell;
        w.update_block_hover();
        if w.hovered_command().map(|c| (c.0, c.1)) != chip_before {
            w.window.request_redraw();
        }

        let modifiers = w.modifiers;
        if let Some((id, (col, row))) = cell
            && let Some(pane) = w.tabs[w.active_tab].pane_mut(id)
        {
            let mode = pane.lock().mode();
            if mode.intersects(TermMode::MOUSE_REPORTING) && !modifiers.shift_key() {
                let action = MouseAction::Motion(pane.held_button);
                if let Some(bytes) = input::encode_mouse(action, None, col, row, modifiers, mode) {
                    pane.write(&bytes);
                }
            } else if pane.selecting {
                let absolute = (pane.lock().viewport_to_absolute(row), col);
                match &mut pane.selection {
                    Some(selection) => selection.head = absolute,
                    None => {
                        let anchor = pane.last_click.map_or(absolute, |(_, pos, _)| pos);
                        pane.selection = Some(SelectionState {
                            anchor,
                            head: absolute,
                            kind: SelectionKind::Simple,
                        });
                    }
                }
                redraw = true;
            }
        }
        if redraw {
            w.window.request_redraw();
        }
    }

    fn handle_wheel(&mut self, delta: MouseScrollDelta) {
        let wheel_lines = self.config.terminal.wheel_scroll_lines as f64;
        let Some(w) = &mut self.window else { return };
        if let Some(picker) = &mut w.palette {
            let lines = match delta {
                MouseScrollDelta::LineDelta(_, dy) => -dy.signum() as isize,
                MouseScrollDelta::PixelDelta(p) => -(p.y.signum() as isize),
            };
            picker.scroll_by(lines);
            w.window.request_redraw();
            return;
        }
        let (x, y) = (w.pointer.x, w.pointer.y);
        let notches = match delta {
            MouseScrollDelta::LineDelta(_, dy) => dy.signum() as isize,
            MouseScrollDelta::PixelDelta(p) => p.y.signum() as isize,
        };
        if w.settings_wheel(notches) || w.files_wheel(x as f32, y as f32, notches) {
            return;
        }
        let m = w.renderer.cell_metrics();
        let modifiers = w.modifiers;
        let Some(id) = w.pane_at(x, y).or(Some(w.active_pane().id)) else { return };
        let Some(pane) = w.tabs[w.active_tab].pane_mut(id) else { return };
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, dy) => f64::from(dy) * wheel_lines,
            MouseScrollDelta::PixelDelta(p) => p.y / f64::from(m.height),
        } + pane.scroll_remainder;
        let whole = lines.trunc() as isize;
        pane.scroll_remainder = lines.fract();
        if whole == 0 {
            return;
        }

        let (col, row) = pane.cell_at(x, y, m);
        let mode = pane.lock().mode();
        if mode.intersects(TermMode::MOUSE_REPORTING) {
            let button = if whole > 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
            for _ in 0..whole.unsigned_abs() {
                if let Some(bytes) = input::encode_mouse(MouseAction::Press, Some(button), col, row, modifiers, mode) {
                    pane.write(&bytes);
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
            pane.write(&key.repeat(whole.unsigned_abs()));
        } else {
            pane.lock().scroll_display(whole);
            pane.scrolled_at = Some(Instant::now());
            pane.update_hover(Some((col, row)), modifiers.control_key());
            w.window.request_redraw();
        }
    }

    //#endregion
}

impl TermWindow {
    fn active_pane(&self) -> &Pane {
        self.tabs[self.active_tab].active_pane()
    }

    /// The split border under a point, if any (a few pixels either side).
    fn border_at(&self, x: f32, y: f32) -> Option<layout::SplitBorder> {
        if self.settings_page().is_some() || self.tab_bar.is_some_and(|b| b.contains(x, y)) {
            return None;
        }
        let slop = (5.0 * self.scale_factor as f32).round();
        let area = self.content_area();
        self.tabs[self.active_tab].split_borders(area).into_iter().find(|b| b.near(x, y, slop))
    }

    /// The visible pane of the active tab containing a point.
    fn pane_at(&self, x: f64, y: f64) -> Option<PaneId> {
        let tab = &self.tabs[self.active_tab];
        tab.panes
            .iter()
            .find(|p| tab.is_visible(p.id) && p.area.contains(x as f32, y as f32))
            .map(|p| p.id)
    }

    fn set_cursor_icon(&mut self, icon: CursorIcon) {
        if icon != self.cursor_icon {
            self.window.set_cursor(icon);
            self.cursor_icon = icon;
        }
    }

    fn update_title(&self) {
        let title = &self.active_pane().title;
        self.window.set_title(if title.is_empty() { "ionterm" } else { title });
    }

    /// Moves keyboard focus to another pane of the active tab, telling both
    /// applications if they asked for focus events.
    fn set_active_pane(&mut self, id: PaneId) {
        let focused = self.focused;
        let tab = &mut self.tabs[self.active_tab];
        if tab.active == id || tab.pane(id).is_none() {
            return;
        }
        if focused {
            send_focus(tab.active_pane(), false);
        }
        tab.active = id;
        if focused {
            send_focus(tab.active_pane(), true);
        }
        self.blink_on = true;
        self.update_title();
        self.window.request_redraw();
    }

    fn select_tab(&mut self, index: usize) {
        if index == self.active_tab || index >= self.tabs.len() {
            return;
        }
        if self.focused {
            send_focus(self.active_pane(), false);
        }
        self.active_tab = index;
        self.tabs[index].activity = Activity::None;
        if self.focused {
            send_focus(self.active_pane(), true);
        }
        self.capture = None;
        self.pointer_cell = None;
        self.update_title();
        self.window.request_redraw();
    }

    /// Places the tab bar and every tab's panes for the current window size.
    /// `force` re-sends sizes even if the grid didn't change (cell size did).
    fn relayout(&mut self, config: &Config, force: bool) {
        let size = self.window.inner_size();
        self.renderer.resize(size.width, size.height);
        let m = self.renderer.cell_metrics();
        let scale = self.scale_factor as f32;
        let (width, height) = (size.width as f32, size.height as f32);

        let show_bar = match config.tab_bar.show {
            ShowTabBar::Always => true,
            ShowTabBar::Never => false,
            ShowTabBar::Auto => self.tabs.len() > 1 || !self.port_chips().is_empty(),
        };
        let mut area = Rect::new(0.0, 0.0, width, height);
        self.tab_bar = None;
        if show_bar {
            let bar_height = tabbar::bar_height(config.tab_bar.style, m);
            area.height -= bar_height;
            self.tab_bar = Some(match config.tab_bar.edge {
                TabBarEdge::Top => {
                    area.y = bar_height;
                    Rect::new(0.0, 0.0, width, bar_height)
                }
                TabBarEdge::Bottom => Rect::new(0.0, height - bar_height, width, bar_height),
            });
        }

        let padding = (config.window.padding * scale).round();
        let border = (config.layouts.border_width * scale).round();
        files::place_panels(&mut self.tabs, area, config, m);
        for tab in &mut self.tabs {
            let bordered = tab.visible_count() > 1;
            let area = tab.pane_area(area);
            for (id, rect) in tab.arrange(area) {
                let inner = if bordered { inset_internal(rect, area, border) } else { rect };
                if let Some(pane) = tab.pane_mut(id) {
                    pane.place(inner, padding, m, force);
                }
            }
        }
        self.pointer_cell = None;
        self.window.request_redraw();
    }

    /// How many rows a new tab's only pane gets (the tab bar shows by then).
    fn new_tab_rows(&self, config: &Config) -> usize {
        let m = self.renderer.cell_metrics();
        let mut height = self.window.inner_size().height as f32;
        if config.tab_bar.show != ShowTabBar::Never {
            height -= tabbar::bar_height(config.tab_bar.style, m);
        }
        height -= 2.0 * (config.window.padding * self.scale_factor as f32).round();
        (height / m.height).floor().max(1.0) as usize
    }

    /// Finds the link under the pointer again: holding ctrl makes file
    /// paths count.
    fn refresh_hover(&mut self) {
        let Some((id, cell)) = self.pointer_cell else { return };
        let ctrl = self.modifiers.control_key();
        let Some(pane) = self.tabs[self.active_tab].pane_mut(id) else { return };
        if pane.update_hover(Some(cell), ctrl) {
            let icon = if pane.hovered.is_some() { CursorIcon::Pointer } else { CursorIcon::Text };
            self.set_cursor_icon(icon);
            self.window.request_redraw();
        }
    }

    /// Turns broadcasting on or off for the active tab.
    fn toggle_broadcast(&mut self) {
        let tab = &mut self.tabs[self.active_tab];
        if !tab.broadcast && tab.panes.len() < 2 {
            self.show_toast("Broadcast needs a split: there's only one pane here");
            return;
        }
        tab.broadcast = !tab.broadcast;
        let message = if tab.broadcast {
            format!("Typing goes to all {} panes (ctrl+shift+b to stop)", tab.panes.len())
        } else {
            "Typing goes to this pane only".to_string()
        };
        self.show_toast(message);
        self.window.request_redraw();
    }

    /// Pastes into a pane (and its broadcast peers), as if typed.
    fn paste_into(&self, id: PaneId, text: &str) {
        let Some(tab) = self.tabs.iter().find(|t| t.pane(id).is_some()) else { return };
        for pane in tab.pane(id).into_iter().chain(tab.input_peers(id)) {
            let mode = pane.lock().mode();
            pane.write(&input::encode_paste(text, mode));
            pane.lock().scroll_to_bottom();
        }
    }

    fn window_rect(&self) -> Rect {
        let size = self.window.inner_size();
        Rect::new(0.0, 0.0, size.width as f32, size.height as f32)
    }

    fn tab_infos(&self, config: &Config) -> Vec<TabInfo> {
        let fallback = default_title(config);
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                let pane = tab.active_pane();
                // SSH tabs are named after the host, plus the remote title.
                let host = pane
                    .connection
                    .as_ref()
                    .map(|c| c.alias.as_str())
                    .or(pane.detected_ssh.as_ref().map(|d| d.host()));
                let extras = host.and_then(|h| self.ssh_extras.get(h));
                let shell_title = (!pane.title.is_empty()).then(|| pane.title.clone());
                let title = tab.custom_title.clone().unwrap_or_else(|| match host {
                    Some(host) => match shell_title {
                        Some(t) if !t.contains(host) => format!("{host}: {t}"),
                        Some(t) => t,
                        None => host.to_string(),
                    },
                    None => shell_title
                        .or_else(|| pane.process.clone())
                        .unwrap_or_else(|| fallback.clone()),
                });
                let icon = match host {
                    Some(_) => extras.and_then(HostExtras::icon_char).unwrap_or(SSH_ICON),
                    None => tabbar::process_icon(pane.process.as_deref()),
                };
                let (title, icon) = settings::tab_title(tab).unwrap_or((title, icon));
                let icon = if tab.broadcast { BROADCAST_ICON } else { icon };
                TabInfo {
                    title,
                    icon,
                    color: tab.color.or(extras.and_then(|e| e.color).map(|c| c.0)),
                    active: i == self.active_tab,
                    progress: self.tab_progress(i),
                    activity: tab.activity,
                }
            })
            .collect()
    }

    fn bar_layout(&self, config: &Config, infos: &[TabInfo]) -> Option<tabbar::BarLayout> {
        let bar = self.tab_bar?;
        let chips: Vec<tabbar::Chip> = self.port_chips().into_iter().map(|(chip, _)| chip).collect();
        Some(tabbar::layout(
            config.tab_bar.style,
            bar,
            infos,
            &chips,
            self.renderer.cell_metrics(),
            self.scale_factor as f32,
            config.tab_bar.max_title_length,
        ))
    }

    /// Advances every tab's drawn position toward its slot and returns the
    /// offsets to draw them at, plus whether anything is still moving.
    fn tab_motion(&mut self, layout: &tabbar::BarLayout) -> (Vec<f32>, bool) {
        let now = Instant::now();
        // The first frame of an animation doesn't move; later ones use the
        // real frame time, capped so a stall doesn't make tabs jump.
        let dt = self
            .last_frame
            .map_or(0.0, |at| now.duration_since(at).as_secs_f32().min(0.05));
        self.last_frame = Some(now);
        let step = 1.0 - (-dt * TAB_GLIDE_RATE).exp();
        let dragged = self.tab_drag.as_ref().filter(|d| d.moved).map(|d| (d.index, d.grab_dx));
        let (first, last) = match (layout.tabs.first(), layout.tabs.last()) {
            (Some(first), Some(last)) => (first.rect, last.rect),
            _ => return (Vec::new(), false),
        };

        let mut moving = dragged.is_some();
        let mut offsets = Vec::with_capacity(self.tabs.len());
        for (i, (tab, slot)) in self.tabs.iter().zip(&layout.tabs).enumerate() {
            let target = slot.rect.x;
            let x = match dragged {
                Some((index, grab_dx)) if index == i => {
                    let max = last.x + last.width - slot.rect.width;
                    ((self.pointer.x - grab_dx) as f32).clamp(first.x, max.max(first.x))
                }
                _ => {
                    let current = self.tab_positions.get(&tab.id).copied().unwrap_or(target);
                    let next = current + (target - current) * step;
                    if (next - target).abs() < 0.5 {
                        target
                    } else {
                        moving = true;
                        next
                    }
                }
            };
            self.tab_positions.insert(tab.id, x);
            offsets.push(x - target);
        }
        let ids: Vec<u64> = self.tabs.iter().map(|t| t.id).collect();
        self.tab_positions.retain(|id, _| ids.contains(id));
        (offsets, moving)
    }

    /// Starts editing a tab's title in place, starting from what it shows now.
    fn start_rename(&mut self, tab: usize, config: &Config) {
        let Some(info) = self.tab_infos(config).into_iter().nth(tab) else { return };
        self.rename = Some(Rename {
            tab,
            field: TextField::new(info.title),
        });
        self.tab_drag = None;
        self.window.request_redraw();
    }

    /// Saves the title being edited; an empty one goes back to the shell's.
    fn commit_rename(&mut self) {
        if let Some(rename) = self.rename.take()
            && let Some(tab) = self.tabs.get_mut(rename.tab)
        {
            let text = rename.field.text().trim().to_string();
            tab.custom_title = (!text.is_empty()).then_some(text);
        }
        self.window.request_redraw();
    }

    /// Handles a key while the picker is open. Returns the chosen action and
    /// whether to open it in a split.
    fn palette_key(&mut self, event: &winit::event::KeyEvent) -> Option<(PaletteAction, bool)> {
        let ctrl = self.modifiers.control_key();
        let picker = self.palette.as_mut()?;
        self.window.request_redraw();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.palette = None,
            Key::Named(NamedKey::Enter) => return picker.selected_action().map(|action| (action, ctrl)),
            Key::Character(c) if ctrl && c.eq_ignore_ascii_case("e") => {
                if let Some(PaletteAction::Connect(alias)) = picker.selected_action() {
                    self.palette = None;
                    return Some((PaletteAction::Edit(alias), false));
                }
            }
            Key::Named(NamedKey::ArrowUp) => picker.move_selection(-1),
            Key::Named(NamedKey::ArrowDown) => picker.move_selection(1),
            Key::Named(NamedKey::Tab) => picker.move_selection(if self.modifiers.shift_key() { -1 } else { 1 }),
            Key::Named(NamedKey::PageUp) => picker.move_selection(-10),
            Key::Named(NamedKey::PageDown) => picker.move_selection(10),
            _ => picker.edit(&event.logical_key, event.text.as_deref(), self.modifiers),
        }
        None
    }

    /// The text field that has the keyboard, if any, following the order
    /// keys are handed out in.
    fn focused_field(&self) -> Option<&TextField> {
        if let Some(open) = &self.dialog {
            return open.input.as_ref();
        }
        if let Some(form) = &self.editor {
            return form.focused_field();
        }
        if let Some(picker) = &self.palette {
            return Some(&picker.query);
        }
        if self.menu.is_some() {
            return None;
        }
        if let Some(rename) = &self.rename {
            return Some(&rename.field);
        }
        if let Some(page) = self.settings_page() {
            let shortcuts = page.on_shortcuts().then_some(&page.shortcut_filter);
            return page.picker.as_ref().map(|p| &p.filter).or(page.typing.as_ref()).or(shortcuts);
        }
        self.search.as_ref().filter(|_| self.search_focused()).map(|s| &s.bar.query)
    }

    /// Feeds a key press to the title being edited.
    fn rename_key(&mut self, event: &winit::event::KeyEvent) {
        match &event.logical_key {
            Key::Named(NamedKey::Enter) => return self.commit_rename(),
            Key::Named(NamedKey::Escape) => self.rename = None,
            _ => {
                if let Some(rename) = &mut self.rename {
                    rename.field.key(&event.logical_key, event.text.as_deref(), self.modifiers);
                }
            }
        }
        self.window.request_redraw();
    }

    /// Border strips: the gaps between each pane's slot and its drawn area.
    fn borders(&self, theme: &Theme, area: Rect) -> Vec<ChromeItem> {
        let tab = &self.tabs[self.active_tab];
        let mut rects = Vec::new();
        if tab.visible_count() < 2 {
            return rects;
        }
        let area = tab.pane_area(area);
        let active = theme.active_border.unwrap_or(theme.tab_bar.active_background).0;
        let inactive = theme.inactive_border.unwrap_or(theme.tab_bar.inactive_background).0;
        // Broadcasting: every pane that gets the typing is outlined in yellow.
        let broadcast = tab.broadcast.then(|| theme.palette.colors.get(3).map_or(active, |c| c.0));
        for (id, slot) in tab.arrange(area) {
            let Some(pane) = tab.pane(id) else { continue };
            let color = match broadcast {
                Some(color) => color,
                None if id == tab.active => active,
                None => inactive,
            };
            let a = pane.area;
            let strips = [
                [slot.x, slot.y, a.x - slot.x, slot.height],
                [a.x + a.width, slot.y, slot.x + slot.width - (a.x + a.width), slot.height],
                [slot.x, slot.y, slot.width, a.y - slot.y],
                [slot.x, a.y + a.height, slot.width, slot.y + slot.height - (a.y + a.height)],
            ];
            rects.extend(
                strips
                    .into_iter()
                    .filter(|s| s[2] > 0.0 && s[3] > 0.0)
                    .map(|rect| ChromeItem::Rect { rect, color, alpha: 1.0 }),
            );
        }
        rects
    }

    fn redraw(&mut self, config: &Config, theme: &Theme) {
        let area = self.content_area();
        let panel_focused = self.panel_focused();
        let settings_shown = self.settings_page().is_some();
        let tab = &self.tabs[self.active_tab];
        let frames: Vec<_> = tab
            .panes
            .iter()
            .filter(|p| tab.is_visible(p.id) && !settings_shown)
            .map(|p| {
                let focused = self.focused && p.id == tab.active && !panel_focused;
                let (snapshot, frame) = p.frame(focused, self.blink_on);
                (snapshot, frame, p.origin, p.area)
            })
            .collect();
        let m = self.renderer.cell_metrics();
        // The keyboard's cursor, in pixels, for the trail.
        let cursor = frames
            .iter()
            .find(|(_, frame, _, _)| frame.focused)
            .and_then(|(snapshot, _, origin, _)| {
                let cursor = snapshot.cursor.as_ref().filter(|_| snapshot.display_offset == 0)?;
                let width = if cursor.wide { 2.0 * m.width } else { m.width };
                Some([origin[0] + cursor.col as f32 * m.width, origin[1] + cursor.row as f32 * m.height, width, m.height])
            });
        let tab_id = tab.id;
        let views: Vec<PaneView> = frames
            .iter()
            .map(|(snapshot, frame, origin, a)| PaneView {
                snapshot,
                origin: *origin,
                area: [a.x, a.y, a.width, a.height],
                frame: *frame,
            })
            .collect();
        let mut items = self.borders(theme, area);
        let infos = self.tab_infos(config);
        let scale = self.scale_factor as f32;
        let mut animating = false;
        if config.cursor.trail {
            let now = Instant::now();
            self.trail.update(tab_id, cursor, now);
            animating |= self.trail.draw(now, theme.cursor.0, &mut items);
        }
        if let Some(layout) = self.bar_layout(config, &infos) {
            let (offsets, still_moving) = self.tab_motion(&layout);
            animating = still_moving;
            let rename = self.rename.as_ref().map(|r| tabbar::RenameView {
                tab: r.tab,
                field: &r.field,
            });
            // The dragged tab, or else the one furthest from home, goes on top.
            let raised = self
                .tab_drag
                .as_ref()
                .filter(|d| d.moved)
                .map(|d| d.index)
                .or_else(|| {
                    offsets
                        .iter()
                        .enumerate()
                        .filter(|(_, dx)| dx.abs() > 0.5)
                        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                        .map(|(i, _)| i)
                });
            tabbar::draw(
                config.tab_bar.style,
                &layout,
                &infos,
                self.tab_hover,
                rename,
                &tabbar::Motion {
                    offsets: &offsets,
                    raised,
                },
                theme,
                config.window.opacity.unwrap_or(theme.background_opacity),
                m,
                scale,
                config.tab_bar.max_title_length,
                integration::progress_phase(),
                &mut items,
            );
            let chips: Vec<tabbar::Chip> = self.port_chips().into_iter().map(|(chip, _)| chip).collect();
            tabbar::draw_chips(&layout, &chips, self.tab_hover, theme, m, &mut items);
            // An indeterminate progress bar's stripe keeps moving.
            animating |= infos.iter().any(|t| t.progress.is_some_and(|p| p.fraction.is_none()));
        }
        self.draw_settings(config, theme, &mut items);
        self.draw_failed_marks(theme, &mut items);
        self.draw_command_info(theme, &mut items);
        self.draw_blocks(config, theme, &mut items);
        self.draw_search(theme, &mut items);
        self.draw_hints(theme, &mut items);
        self.draw_files(theme, &mut items);
        self.draw_drag(theme, &mut items);
        if let Some(open) = &self.menu {
            let layout = menu::layout(open, theme, m, scale, self.window_rect());
            menu::draw(open, &layout, theme, m, scale, &mut items);
        }
        if let Some(picker) = &self.palette {
            let layout = palette::layout(picker, m, scale, self.window_rect());
            palette::draw(picker, &layout, theme, m, scale, &mut items);
        }
        if let Some(form) = &self.editor {
            let layout = editor::layout(form, m, scale, self.window_rect());
            editor::draw(form, &layout, theme, m, scale, &mut items);
        }
        if let Some(open) = &self.dialog {
            let layout = dialog::layout(open, m, scale, self.window_rect());
            dialog::draw(open, &layout, theme, m, scale, &mut items);
        }
        // Asks the compositor for a frame callback, so a redraw requested for
        // the next animation frame arrives at the display's refresh rate.
        self.window.pre_present_notify();
        self.renderer.render(
            &views,
            &Chrome {
                background: theme.background.0,
                items: &items,
            },
        );
        if animating {
            // Next frame; presentation is vsync-paced, so this runs at the
            // display's refresh rate.
            self.window.request_redraw();
        } else {
            self.last_frame = None;
        }
    }
}

/// The window icon, for platforms that take it from the app (X11, Windows).
/// Wayland compositors use the `.desktop` file's icon instead.
fn window_icon() -> Option<winit::window::Icon> {
    const PNG: &[u8] = include_bytes!("../../../data/icons/hicolor/64x64/apps/ionterm.png");
    let mut decoder = png::Decoder::new(std::io::Cursor::new(PNG));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut rgba = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut rgba).ok()?;
    rgba.truncate(info.buffer_size());
    if info.color_type != png::ColorType::Rgba {
        return None;
    }
    winit::window::Icon::from_rgba(rgba, info.width, info.height).ok()
}

/// Shrinks a pane's slot by `border` on the sides that face another pane,
/// leaving the window's outer edges borderless.
fn inset_internal(rect: Rect, area: Rect, border: f32) -> Rect {
    let left = if rect.x > area.x + 0.5 { border } else { 0.0 };
    let top = if rect.y > area.y + 0.5 { border } else { 0.0 };
    let right = if rect.x + rect.width < area.x + area.width - 0.5 { border } else { 0.0 };
    let bottom = if rect.y + rect.height < area.y + area.height - 0.5 { border } else { 0.0 };
    Rect::new(
        rect.x + left,
        rect.y + top,
        (rect.width - left - right).max(1.0),
        (rect.height - top - bottom).max(1.0),
    )
}

fn send_focus(pane: &Pane, focused: bool) {
    if pane.lock().mode().contains(TermMode::FOCUS_EVENTS) {
        pane.write(if focused { b"\x1b[I" } else { b"\x1b[O" });
    }
}

/// What a tab is called before its shell sets a title: the program's name.
fn default_title(config: &Config) -> String {
    let program = config
        .terminal
        .command
        .first()
        .cloned()
        .or_else(|| config.terminal.shell.split_whitespace().next().map(String::from))
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "shell".into());
    std::path::Path::new(&program)
        .file_name()
        .map_or(program.clone(), |name| name.to_string_lossy().into_owned())
}

fn font_options(font: &FontConfig, size_pt: f32, scale_factor: f64) -> FontOptions {
    FontOptions {
        family: font.family.clone(),
        ligatures: font.ligatures,
        // Points to pixels at 96 DPI, then the output's scale.
        size: size_pt * 96.0 / 72.0 * scale_factor as f32,
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
                .is_some_and(|name| name == CONFIG_FILE || name == THEME_FILE || name == ssh::HOSTS_FILE)
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
        if std::mem::replace(&mut self.started, true) {
            return;
        }
        self.open_pending_windows(event_loop);
        if self.window.is_none() {
            event_loop.exit();
            return;
        }
        if self.config.terminal.remote_control {
            let proxy = self.proxy.clone();
            crate::remote::listen(move |request| proxy.send_event(UserEvent::Remote(request)).is_ok());
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // A tab let go outside its window lands in the window that gets the
        // pointer next.
        if let WindowEvent::CursorMoved { position, .. } = &event
            && self.tab_drop.is_some()
        {
            self.land_tab_drop(id, position.x);
        }
        if !self.select_window(id) {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                if self.parked.is_empty() {
                    // Closing the last window keeps the session for next time.
                    self.save_session(Instant::now(), true);
                    event_loop.exit();
                } else {
                    self.close_window();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(w) = &mut self.window {
                    w.redraw(&self.config, &self.theme);
                }
            }
            WindowEvent::Resized(_) => {
                if let Some(w) = &mut self.window {
                    w.relayout(&self.config, false);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let style = self.render_style();
                let Some(w) = &mut self.window else { return };
                w.scale_factor = scale_factor;
                w.renderer.set_style(style);
                w.renderer.set_font(font_options(&self.config.font, w.font_size, scale_factor));
                w.relayout(&self.config, true);
            }
            WindowEvent::Focused(focused) => {
                if focused {
                    self.last_focused = Some(id);
                }
                if let Some(w) = &mut self.window {
                    w.focused = focused;
                    w.blink_on = true;
                    send_focus(w.active_pane(), focused);
                    w.window.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                if let Some(w) = &mut self.window {
                    let ctrl_changed = w.modifiers.control_key() != modifiers.state().control_key();
                    w.modifiers = modifiers.state();
                    if ctrl_changed {
                        w.refresh_hover();
                    }
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => {
                let blink = self.blink_interval();
                if self.block_menu_key(&event) {
                    return;
                }
                let Some(w) = &mut self.window else { return };
                // Hints take every key while shown (ctrl+shift+e again hides them).
                if w.hints.is_some() {
                    let toggle = KeyCombo::from_event(&event, w.modifiers)
                        .and_then(|combo| self.bindings.get(&combo).copied())
                        == Some(Action::Hints);
                    if event.state == ElementState::Pressed {
                        if toggle {
                            self.toggle_hints();
                        } else {
                            self.hints_key(&event);
                        }
                    }
                    return;
                }
                // The clipboard keys belong to a text field while one has the keyboard.
                if event.state == ElementState::Pressed && self.field_clipboard_key(&event) {
                    return;
                }
                let Some(w) = &mut self.window else { return };
                if let Some(open) = &mut w.dialog {
                    if event.state != ElementState::Pressed {
                        return;
                    }
                    w.window.request_redraw();
                    if let Some(answer) = open.key(&event.logical_key, event.text.as_deref(), w.modifiers) {
                        return self.dialog_answer(answer);
                    }
                    return;
                }
                if let Some(form) = &mut w.editor {
                    if event.state != ElementState::Pressed {
                        return;
                    }
                    w.window.request_redraw();
                    if let Some(outcome) = form.key(&event.logical_key, event.text.as_deref(), w.modifiers) {
                        return self.editor_outcome(outcome);
                    }
                    return;
                }
                if w.palette.is_some() {
                    if event.state == ElementState::Pressed
                        && let Some((action, split)) = w.palette_key(&event)
                    {
                        self.run_palette(action, split);
                    }
                    return;
                }
                if w.menu.is_some() {
                    if event.state == ElementState::Pressed && event.logical_key == Key::Named(NamedKey::Escape) {
                        w.menu = None;
                        w.window.request_redraw();
                    }
                    return;
                }
                if w.rename.is_some() {
                    if event.state == ElementState::Pressed {
                        w.rename_key(&event);
                    }
                    return;
                }
                let mods = w.modifiers;
                // Caret keys stay in a text field, even when they're bound
                // (ctrl+shift+left selects a word rather than switching tabs).
                let field_key = w.focused_field().is_some()
                    && matches!(
                        event.logical_key,
                        Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight | NamedKey::Home | NamedKey::End | NamedKey::Backspace | NamedKey::Delete)
                    );
                if event.state == ElementState::Pressed
                    && !field_key
                    && let Some(action) = KeyCombo::from_event(&event, mods).and_then(|c| self.bindings.get(&c))
                {
                    let action = *action;
                    return self.perform(action);
                }
                if w.settings_page().is_some() {
                    if event.state == ElementState::Pressed {
                        self.settings_key(&event);
                    }
                    return;
                }
                if w.search_focused() {
                    if event.state == ElementState::Pressed {
                        self.search_key(&event);
                    }
                    return;
                }
                if w.panel_focused() {
                    if event.state == ElementState::Pressed {
                        self.files_key(&event);
                    }
                    return;
                }
                let pane = w.active_pane();
                if pane.disconnected.is_some() {
                    if event.state == ElementState::Pressed && event.logical_key == Key::Named(NamedKey::Enter) {
                        self.reconnect();
                    }
                    return;
                }
                // Keys would corrupt a ZMODEM transfer; ctrl+c or Escape stops it.
                if pane.zmodem.is_active() {
                    let stop = event.logical_key == Key::Named(NamedKey::Escape)
                        || (mods.control_key() && matches!(&event.logical_key, Key::Character(c) if c.eq_ignore_ascii_case("c")));
                    if event.state == ElementState::Pressed && stop {
                        let id = pane.id;
                        w.cancel_zmodem(id);
                        w.show_toast("Cancelling the transfer…");
                    }
                    return;
                }
                let (mode, flags) = {
                    let term = pane.lock();
                    (term.mode(), term.keyboard_flags())
                };
                if let Some(bytes) = input::encode_key(&event, mods, mode, flags) {
                    pane.write(&bytes);
                    // Each peer gets the key as its own modes encode it.
                    for peer in w.tabs[w.active_tab].input_peers(pane.id) {
                        let (mode, flags) = {
                            let term = peer.lock();
                            (term.mode(), term.keyboard_flags())
                        };
                        if let Some(bytes) = input::encode_key(&event, mods, mode, flags) {
                            peer.write(&bytes);
                            if event.state == ElementState::Pressed {
                                peer.lock().scroll_to_bottom();
                            }
                        }
                    }
                    if event.state == ElementState::Pressed {
                        pane.lock().scroll_to_bottom();
                        w.blink_on = true;
                        w.next_blink = blink.map(|d| Instant::now() + d);
                        w.window.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => self.handle_mouse_button(button, state),
            WindowEvent::CursorMoved { position, .. } => self.handle_cursor_moved(position),
            WindowEvent::HoveredFile(_) => self.drag_event(DragEvent::Hover(None)),
            WindowEvent::HoveredFileCancelled => self.drag_event(DragEvent::Left),
            WindowEvent::DroppedFile(path) => {
                if let Some(w) = &mut self.window {
                    w.dropped.push(path);
                }
            }
            WindowEvent::CursorLeft { .. } => {
                if let Some(w) = &mut self.window {
                    let mut redraw = false;
                    for pane in &mut w.tabs[w.active_tab].panes {
                        redraw |= pane.update_hover(None, false);
                    }
                    w.pointer_cell = None;
                    if w.tab_hover != Hit::None {
                        w.tab_hover = Hit::None;
                        redraw = true;
                    }
                    if redraw {
                        w.window.request_redraw();
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => self.handle_wheel(delta),
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        // Each event goes to the window it's about.
        match event {
            UserEvent::Wakeup(id) => {
                if self.select_window_of_pane(id) {
                    self.handle_wakeup(id);
                }
            }
            UserEvent::ChildExited(id) => {
                if self.select_window_of_pane(id) {
                    self.child_exited(id);
                }
            }
            UserEvent::ConfigChanged => self.reload_at = Some(Instant::now() + RELOAD_DEBOUNCE),
            UserEvent::Files(event) => {
                if self.select_window_of_pane(event.pane()) {
                    self.files_event(event);
                }
            }
            UserEvent::Transfer(id, event) => {
                let window = self.windows().find(|w| w.transfers.list.iter().any(|t| t.id == id)).map(|w| w.id);
                if window.is_some_and(|window| self.select_window(window)) {
                    self.transfer_event(id, event);
                }
            }
            UserEvent::Drag(surface, event) => {
                let window = self.windows().find(|w| surface.is_some() && w.surface == surface).map(|w| w.id);
                if window.is_some_and(|window| self.select_window(window)) {
                    self.drag_event(event);
                }
            }
            UserEvent::Zmodem(pane, event) => {
                if self.select_window_of_pane(pane) {
                    self.zmodem_event(pane, event);
                }
            }
            UserEvent::Remote(request) => self.remote_request(request),
            UserEvent::Edit(id, event) => self.edit_event(id, event),
            UserEvent::Forward(pane, event) => {
                if self.select_window_of_pane(pane) {
                    self.forward_event(pane, event);
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.started {
            self.open_pending_windows(event_loop);
        }
        if self.exit_requested {
            event_loop.exit();
            return;
        }
        let now = Instant::now();
        self.finish_tab_drop(now);
        if self.reload_at.is_some_and(|at| at <= now) {
            self.reload_at = None;
            self.reload_config();
        }
        let session_wake = self.save_session(now, false);
        let edit_wake = self.edit_timers(now);
        let mut wake = [self.reload_at, session_wake, self.tab_drop_deadline(), edit_wake].into_iter().flatten().min();
        self.for_each_window(|app| {
            let next = app.window_timers(now);
            wake = [wake, next].into_iter().flatten().min();
        });
        // Something above may have opened or closed a window.
        if !self.pending_windows.is_empty() {
            wake = Some(now);
        }
        if self.exit_requested {
            event_loop.exit();
            return;
        }
        event_loop.set_control_flow(match wake {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // The drop listener shares winit's Wayland connection: stop its
        // thread now, while the connection is still open.
        self.drops = None;
        scrollback::remove_files();
        crate::edit::remove_local_dirs();
        crate::remote::remove_socket();
    }
}

impl App {
    /// The window being worked on's timed work: dropped files, finished
    /// transfers and progress, search, synchronized output, selection
    /// auto-scroll and the cursor blink. Returns when to come back.
    fn window_timers(&mut self, now: Instant) -> Option<Instant> {
        self.flush_dropped();
        let blink = self.blink_interval();
        let listening_wake = self.window.as_mut().and_then(|w| w.refresh_listening(&self.config, now));
        let progress_wake = self.window.as_mut().and_then(|w| w.expire_progress(now));
        let search_wake = self.window.as_mut().and_then(|w| w.refresh_search(now));
        let scrollbar_wake = self.window.as_mut().and_then(|w| w.expire_scrollbars(now));
        let mut wake = [self.expire_files(now), progress_wake, search_wake, listening_wake, scrollbar_wake].into_iter().flatten().min();
        if let Some(w) = &mut self.window {
            let m = w.renderer.cell_metrics();
            let pointer = (w.pointer.x, w.pointer.y);
            let capture = w.capture;
            let mut redraw = false;
            for pane in &mut w.tabs[w.active_tab].panes {
                if pane.sync_deadline.is_some_and(|at| at <= now) {
                    pane.sync_deadline = None;
                    redraw = true;
                }
                wake = [wake, pane.sync_deadline].into_iter().flatten().min();
                if capture == Some(pane.id) {
                    let (next, moved) = pane.autoscroll(now, pointer, m);
                    redraw |= moved;
                    wake = [wake, next].into_iter().flatten().min();
                }
            }
            match (blink, w.focused) {
                (Some(interval), true) => {
                    let next = *w.next_blink.get_or_insert(now + interval);
                    if next <= now {
                        w.blink_on = !w.blink_on;
                        w.next_blink = Some(now + interval);
                        redraw = true;
                    }
                }
                _ => w.next_blink = None,
            }
            wake = [wake, w.next_blink].into_iter().flatten().min();
            if redraw {
                w.window.request_redraw();
            }
        }
        wake
    }
}
