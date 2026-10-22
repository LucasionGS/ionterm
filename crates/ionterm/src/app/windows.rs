//! Several windows in one ionterm: opening and closing them, moving tabs
//! and panes between them, and dragging a tab out of its window.
//!
//! `App::window` is the window being worked on and `App::parked` holds the
//! others. Every event first makes its own window the current one
//! (`select_window`), so the rest of the app only ever deals with one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use ionterm_render::Renderer;
use winit::dpi::PhysicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::ModifiersState;
use winit::window::{CursorIcon, Window, WindowId};

use super::{App, TermWindow, background, font_options, send_focus, trail, window_icon};
use crate::config::Decorations;
use crate::dnd;
use crate::files::transfer::{Transfer, TransferId, Transfers};
use crate::layout::{Direction, PaneId};
use crate::palette::{Palette, PaletteAction, PaletteItem, PaneTarget};
use crate::pane::Pane;
use crate::remote::Request;
use crate::session::SavedWindow;
use crate::ssh::{self, SshArgs};
use crate::tab::Tab;
use crate::tabbar::{self, Hit};

/// How long a tab dropped outside its window waits for another ionterm
/// window to report the pointer (the drop landed there) before it opens a
/// window of its own.
const TAB_DROP_WAIT: Duration = Duration::from_millis(250);

/// A window to open once the event loop can (`about_to_wait`).
pub struct NewWindow {
    pub content: Content,
    /// A name for its first tab.
    pub title: Option<String>,
    /// The activation token of the program that asked for the window
    /// (`XDG_ACTIVATION_TOKEN`), so the compositor hands it the keyboard.
    pub activation_token: Option<String>,
    /// A remote `new-window` request, answered with the new window's pane.
    pub reply: Option<Request>,
    /// Transfers of the panes moving into it.
    pub(super) carried: Carried,
}

impl NewWindow {
    pub fn new(content: Content) -> Self {
        Self {
            content,
            title: None,
            activation_token: None,
            reply: None,
            carried: Carried::default(),
        }
    }
}

/// What a new window opens with.
pub enum Content {
    /// A shell in `cwd`. The first such window restores the saved session
    /// instead, when there is one and `restore` allows it (not for `ionterm -d`).
    Shell { cwd: Option<PathBuf>, restore: bool },
    /// A program instead of the shell (`ionterm -e`).
    Command { argv: Vec<String>, cwd: Option<PathBuf> },
    /// A connection to an ssh config host (`ionterm host`).
    Host(String),
    /// A connection with ssh arguments (`ionterm ssh`).
    Ssh(SshArgs),
    /// Tabs moved from another window, and the one to show.
    Tabs(Vec<Tab>, usize),
    /// A window of the saved session.
    Restore(SavedWindow),
}

/// What travels with panes to another window: their transfers, which the
/// window lists and draws.
#[derive(Default)]
pub(super) struct Carried {
    transfers: Vec<Transfer>,
    zmodem: Vec<(PaneId, TransferId)>,
}

/// A tab dragged out of its window. Releasing the pointer over another
/// window moves the pointer's focus there, so if that window reports the
/// pointer soon after, the tab moves into it; otherwise it gets a new one.
pub(super) struct TabDrop {
    from: WindowId,
    tab: u64,
    until: Instant,
}

impl TermWindow {
    /// Takes out the transfers of panes leaving this window.
    fn carry_out(&mut self, panes: &[PaneId]) -> Carried {
        let mut carried = Carried::default();
        for &pane in panes {
            carried.transfers.extend(self.transfers.take_for_pane(pane));
            if let Some(id) = self.zmodem_transfers.remove(&pane) {
                carried.zmodem.push((pane, id));
            }
        }
        // Overlays about a pane that leaves go away.
        if self.search.as_ref().is_some_and(|s| panes.contains(&s.pane)) {
            self.search = None;
        }
        if self.hints.as_ref().is_some_and(|h| panes.contains(&h.pane)) {
            self.hints = None;
        }
        if self.capture.is_some_and(|id| panes.contains(&id)) {
            self.capture = None;
        }
        self.pointer_cell = None;
        carried
    }

    fn carry_in(&mut self, carried: Carried) {
        self.transfers.adopt(carried.transfers);
        self.zmodem_transfers.extend(carried.zmodem);
    }

    /// The window as the pickers name it: its active tab's title.
    fn describe(&self, config: &crate::config::Config) -> String {
        let infos = self.tab_infos(config);
        let title = infos.get(self.active_tab).map(|i| i.title.clone()).unwrap_or_default();
        match self.tabs.len() {
            1 => title,
            n => format!("{title} · {n} tabs"),
        }
    }
}

impl App {
    //#region Choosing the window

    /// Makes a window the one being worked on. False when it's gone.
    pub(super) fn select_window(&mut self, id: WindowId) -> bool {
        if self.window.as_ref().is_some_and(|w| w.id == id) {
            return true;
        }
        let Some(index) = self.parked.iter().position(|w| w.id == id) else { return false };
        let window = self.parked.swap_remove(index);
        if let Some(old) = self.window.replace(window) {
            self.parked.push(old);
        }
        true
    }

    /// Makes the window showing a pane the one being worked on.
    pub(super) fn select_window_of_pane(&mut self, pane: PaneId) -> bool {
        let found = self.windows().find(|w| w.tabs.iter().any(|t| t.pane(pane).is_some())).map(|w| w.id);
        found.is_some_and(|id| self.select_window(id))
    }

    /// Every window, the current one first.
    pub(super) fn windows(&self) -> impl Iterator<Item = &TermWindow> {
        self.window.iter().chain(&self.parked)
    }

    /// Every window, in the order they were opened.
    pub(super) fn window_order(&self) -> Vec<WindowId> {
        let mut all: Vec<(u64, WindowId)> = self.windows().map(|w| (w.serial, w.id)).collect();
        all.sort_by_key(|(serial, _)| *serial);
        all.into_iter().map(|(_, id)| id).collect()
    }

    /// Runs `work` with each window in turn as the current one, then makes
    /// the one that was current before current again.
    pub(super) fn for_each_window(&mut self, mut work: impl FnMut(&mut Self)) {
        let current = self.window.as_ref().map(|w| w.id);
        for id in self.window_order() {
            if self.select_window(id) {
                work(self);
            }
        }
        if let Some(id) = current {
            self.select_window(id);
        }
    }

    //#endregion

    //#region Opening and closing

    /// Opens a window on the next pass of the event loop.
    pub(super) fn request_window(&mut self, new: NewWindow) {
        self.pending_windows.push(new);
    }

    /// A new window with a shell in the current pane's directory.
    pub(super) fn new_window(&mut self) {
        let cwd = self.window.as_ref().and_then(|w| w.active_pane().working_dir());
        self.request_window(NewWindow::new(Content::Shell { cwd, restore: true }));
    }

    /// Opens the windows asked for since the last pass. If the first window
    /// can't open, the app ends with the error.
    pub(super) fn open_pending_windows(&mut self, event_loop: &ActiveEventLoop) {
        while !self.pending_windows.is_empty() {
            let new = self.pending_windows.remove(0);
            if let Err(error) = self.create_window(event_loop, new) {
                if self.window.is_none() {
                    self.error = Some(error);
                    self.exit_requested = true;
                    return;
                }
                tracing::warn!("could not open a window: {error:#}");
                if let Some(w) = &mut self.window {
                    w.show_toast(format!("Couldn't open a window: {error}"));
                }
            }
        }
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop, new: NewWindow) -> anyhow::Result<()> {
        let NewWindow {
            content,
            title,
            activation_token,
            reply,
            carried,
        } = new;
        let (window, renderer) = match self.window_parts(event_loop, activation_token) {
            Ok(parts) => parts,
            Err(error) => {
                // Moved tabs go back where they came from rather than die.
                if let Content::Tabs(tabs, _) = content {
                    for tab in tabs {
                        self.adopt_tab(tab, Carried::default(), None);
                    }
                    if let Some(w) = &mut self.window {
                        w.carry_in(carried);
                    }
                }
                if let Some(reply) = reply {
                    reply.reply(1, format!("Couldn't open the window: {error:#}"));
                }
                return Err(error);
            }
        };
        self.gpu.get_or_insert_with(|| renderer.gpu().clone());
        let m = renderer.cell_metrics();
        let mut in_session = true;
        let opened = match content {
            Content::Tabs(tabs, active) => Ok((tabs, active)),
            Content::Restore(saved) => match self.restore_window(saved, m) {
                Some(restored) => Ok(restored),
                None => self.spawn_pane(m, None, None).map(|p| self.single_tab(p)),
            },
            Content::Shell { cwd, restore } => {
                let saved = if restore { self.take_saved_session() } else { None };
                let restored = saved.and_then(|mut windows| {
                    if windows.is_empty() {
                        return None;
                    }
                    let first = windows.remove(0);
                    self.pending_windows
                        .extend(windows.into_iter().map(|w| NewWindow::new(Content::Restore(w))));
                    self.restore_window(first, m)
                });
                match restored {
                    Some(restored) => Ok(restored),
                    None => self.spawn_pane(m, None, cwd).map(|p| self.single_tab(p)),
                }
            }
            // Windows opened for one thing aren't part of the session.
            Content::Command { argv, cwd } => {
                in_session = false;
                self.spawn_command_pane(m, argv, cwd).map(|p| self.single_tab(p))
            }
            Content::Host(alias) => {
                in_session = false;
                let connection = self.connection_for(&alias);
                self.spawn_pane(m, Some(connection), None).map(|p| self.single_tab(p))
            }
            Content::Ssh(args) => {
                in_session = false;
                let connection = self.connection_from_args(&args);
                self.spawn_pane(m, Some(connection), None).map(|p| self.single_tab(p))
            }
        };
        let (mut tabs, active_tab) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                if let Some(reply) = reply {
                    reply.reply(1, format!("Couldn't open the window: {error:#}"));
                }
                return Err(error);
            }
        };
        let active_tab = active_tab.min(tabs.len().saturating_sub(1));
        if let Some(title) = title.filter(|t| !t.is_empty()) {
            tabs[active_tab].custom_title = Some(title);
        }
        let scale_factor = window.scale_factor();
        let serial = self.next_window_serial;
        self.next_window_serial += 1;
        let mut w = TermWindow {
            id: window.id(),
            serial,
            in_session,
            surface: dnd::surface_key(&window),
            window,
            renderer,
            tabs,
            active_tab,
            scale_factor,
            font_size: self.config.font.size,
            modifiers: ModifiersState::empty(),
            focused: true,
            blink_on: true,
            next_blink: self.blink_interval().map(|d| Instant::now() + d),
            pointer: winit::dpi::PhysicalPosition::new(0.0, 0.0),
            capture: None,
            pointer_cell: None,
            cursor_icon: CursorIcon::Text,
            tab_bar: None,
            tab_hover: Hit::None,
            tab_drag: None,
            rename: None,
            menu: None,
            last_tab_click: None,
            tab_positions: HashMap::new(),
            palette: None,
            editor: None,
            ssh_extras: ssh::load_extras(),
            last_frame: None,
            dialog: None,
            transfers: Transfers::new(self.proxy.clone()),
            toast: None,
            card_hover: None,
            file_drag: None,
            dropped: Vec::new(),
            zmodem_transfers: HashMap::new(),
            hints: None,
            search: None,
            split_drag: None,
            trail: trail::Trail::default(),
            listening_checked: None,
            chips_shown: false,
            block_menu: None,
            block_hover: None,
            block_handle_hovered: false,
            scrollbar_hover: None,
            scrollbar_drag: None,
            sticky_hover: None,
        };
        w.carry_in(carried);
        if self.drops.is_none() {
            self.drops = dnd::listen(&w.window, self.proxy.clone());
        }
        match background::load(&self.config.window) {
            Ok(picture) => w.renderer.set_background(picture),
            Err(message) => {
                tracing::warn!("{message}");
                w.show_toast(message);
            }
        }
        // Panes that moved in were sized for their old window.
        w.relayout(&self.config, true);
        let pane = w.active_pane().id;
        let panes: Vec<PaneId> = w.tabs.iter().flat_map(|t| t.panes.iter().map(|p| p.id)).collect();
        if let Some(old) = self.window.replace(w) {
            self.parked.push(old);
        }
        self.catch_up(&panes);
        if let Some(reply) = reply {
            reply.reply(0, format!("{pane}\n"));
        }
        Ok(())
    }

    /// The window and its renderer, sized to the configured cells.
    fn window_parts(&self, event_loop: &ActiveEventLoop, activation_token: Option<String>) -> anyhow::Result<(Arc<Window>, Renderer)> {
        let attributes = Window::default_attributes()
            .with_title("ionterm")
            .with_transparent(true)
            .with_decorations(match self.config.window.decorations {
                Decorations::Full => true,
                Decorations::None => false,
                // Tiling compositors draw their own borders.
                Decorations::Auto => std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_none(),
            });
        // The app id Hyprland window rules match on, and the token that lets
        // a window opened for another program take the keyboard.
        #[cfg(all(unix, not(target_os = "macos")))]
        let attributes = {
            use winit::platform::startup_notify::WindowAttributesExtStartupNotify;
            use winit::platform::wayland::WindowAttributesExtWayland;
            let attributes = attributes.with_name("ionterm", "ionterm");
            match activation_token {
                Some(token) => attributes.with_activation_token(winit::window::ActivationToken::from_raw(token)),
                None => attributes,
            }
        };
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let _ = activation_token;
        let window = Arc::new(event_loop.create_window(attributes.with_window_icon(window_icon())).context("creating the window")?);
        window.set_cursor(CursorIcon::Text);

        let scale_factor = window.scale_factor();
        let size = window.inner_size();
        let renderer = Renderer::new(
            window.clone(),
            self.gpu.as_ref(),
            event_loop.owned_display_handle(),
            size.width,
            size.height,
            font_options(&self.config.font, self.config.font.size, scale_factor),
            self.render_style(),
        )?;

        // Ask for the configured size in cells; tiling compositors may ignore it.
        let m = renderer.cell_metrics();
        let pad = (self.config.window.padding * scale_factor as f32).round();
        let _ = window.request_inner_size(PhysicalSize::new(
            (f32::from(self.config.window.columns) * m.width + 2.0 * pad) as u32,
            (f32::from(self.config.window.lines) * m.height + 2.0 * pad) as u32,
        ));
        Ok((window, renderer))
    }

    fn single_tab(&self, pane: Pane) -> (Vec<Tab>, usize) {
        (vec![Tab::new(pane, self.default_layout())], 0)
    }

    /// Handles what panes did while they were between windows: output whose
    /// wakeup found no window, and programs that ended meanwhile.
    fn catch_up(&mut self, panes: &[PaneId]) {
        for &id in panes {
            if !self.select_window_of_pane(id) {
                continue;
            }
            self.handle_wakeup(id);
            let ended = self
                .window
                .as_mut()
                .and_then(|w| w.tabs.iter_mut().find_map(|t| t.pane_mut(id)))
                .is_some_and(|p| !p.is_running());
            if ended {
                self.child_exited(id);
            }
        }
    }

    /// Closes the window being worked on, ending the shells it still has,
    /// and makes another one current. The last window ends the app.
    pub(super) fn close_window(&mut self) {
        let Some(w) = self.window.take() else { return };
        if self.last_focused == Some(w.id) {
            self.last_focused = None;
        }
        drop(w);
        self.window = self.parked.pop();
        if self.window.is_none() && self.pending_windows.is_empty() {
            self.exit_requested = true;
        }
    }

    //#endregion

    //#region Moving tabs and panes

    /// Takes a tab out of the window being worked on, with what its panes
    /// carry. The window closes when that was its last tab.
    fn take_tab(&mut self, index: usize) -> Option<(Tab, Carried)> {
        let w = self.window.as_mut()?;
        if index >= w.tabs.len() {
            return None;
        }
        let tab = w.tabs.remove(index);
        let ids: Vec<PaneId> = tab.panes.iter().map(|p| p.id).collect();
        let carried = w.carry_out(&ids);
        w.rename = None;
        w.menu = None;
        w.tab_drag = None;
        w.tab_hover = Hit::None;
        w.split_drag = None;
        if w.tabs.is_empty() {
            self.close_window();
            return Some((tab, carried));
        }
        if w.active_tab > index || w.active_tab >= w.tabs.len() {
            w.active_tab = w.active_tab.saturating_sub(1);
        }
        if w.focused {
            send_focus(w.active_pane(), true);
        }
        w.update_title();
        w.relayout(&self.config, false);
        Some((tab, carried))
    }

    /// Puts a tab into the window being worked on (at `index`, else at the
    /// end), shows it and brings the window forward.
    fn adopt_tab(&mut self, tab: Tab, carried: Carried, index: Option<usize>) {
        let Some(w) = &mut self.window else { return };
        let index = index.unwrap_or(w.tabs.len()).min(w.tabs.len());
        let panes: Vec<PaneId> = tab.panes.iter().map(|p| p.id).collect();
        w.tabs.insert(index, tab);
        if index <= w.active_tab && w.tabs.len() > 1 {
            w.active_tab += 1;
        }
        w.carry_in(carried);
        w.select_tab(index);
        w.relayout(&self.config, true);
        w.window.focus_window();
        self.catch_up(&panes);
    }

    /// Moves a tab of the window being worked on to another window, or a
    /// new one, keeping its shells, scrollback and connections.
    pub(super) fn move_tab(&mut self, index: usize, to: Option<WindowId>) {
        self.move_tab_to_slot(index, to, None);
    }

    fn move_tab_to_slot(&mut self, index: usize, to: Option<WindowId>, slot: Option<usize>) {
        let Some(w) = &mut self.window else { return };
        if to == Some(w.id) || index >= w.tabs.len() {
            return;
        }
        if to.is_none() && w.tabs.len() == 1 {
            w.show_toast("That's the only tab in this window");
            return;
        }
        if let Some(target) = to
            && !self.parked.iter().any(|p| p.id == target)
        {
            return;
        }
        let Some((tab, carried)) = self.take_tab(index) else { return };
        match to {
            Some(target) => {
                self.select_window(target);
                self.adopt_tab(tab, carried, slot);
            }
            None => self.request_window(NewWindow {
                carried,
                ..NewWindow::new(Content::Tabs(vec![tab], 0))
            }),
        }
    }

    /// Moves a pane out of its tab: into a tab of its own, a window of its
    /// own, or another tab (in any window) as a split.
    pub(super) fn move_pane(&mut self, id: PaneId, target: PaneTarget) {
        if !self.select_window_of_pane(id) {
            return;
        }
        let layout = self.default_layout();
        let Some(w) = &mut self.window else { return };
        let Some(tab_index) = w.tabs.iter().position(|t| t.pane(id).is_some()) else { return };
        let alone = w.tabs[tab_index].panes.len() == 1;
        match target {
            PaneTarget::NewTab if alone => return w.show_toast("That pane already has the tab to itself"),
            PaneTarget::NewWindow if alone => return self.move_tab(tab_index, None),
            PaneTarget::Tab(tab) if w.tabs[tab_index].id == tab => return,
            _ => {}
        }
        // Keep the source window from closing until the pane has landed.
        let Some((pane, carried)) = self.take_pane(tab_index, id) else { return };
        match target {
            PaneTarget::NewTab => {
                let at = self.window.as_ref().map(|w| w.active_tab + 1);
                self.adopt_tab(Tab::new(pane, layout), carried, at);
            }
            PaneTarget::NewWindow => self.request_window(NewWindow {
                carried,
                ..NewWindow::new(Content::Tabs(vec![Tab::new(pane, layout)], 0))
            }),
            PaneTarget::Tab(tab) => {
                let window = self.windows().find(|w| w.tabs.iter().any(|t| t.id == tab)).map(|w| w.id);
                if let Some(window) = window {
                    self.select_window(window);
                }
                let Some(w) = &mut self.window else { return };
                let Some(index) = w.tabs.iter().position(|t| t.id == tab) else {
                    // The tab closed meanwhile: the pane gets its own.
                    return self.adopt_tab(Tab::new(pane, layout), carried, None);
                };
                let active = w.tabs[index].active_pane();
                let direction = if active.area.width >= active.area.height { Direction::Horizontal } else { Direction::Vertical };
                w.tabs[index].add(pane, direction);
                w.carry_in(carried);
                w.select_tab(index);
                w.set_active_pane(id);
                w.relayout(&self.config, true);
                w.window.focus_window();
                self.catch_up(&[id]);
            }
        }
    }

    /// Takes a pane out of a tab of the window being worked on; a tab left
    /// empty closes, and so does a window left without tabs.
    fn take_pane(&mut self, tab_index: usize, id: PaneId) -> Option<(Pane, Carried)> {
        let w = self.window.as_mut()?;
        if w.tabs[tab_index].panes.len() == 1 {
            let (mut tab, carried) = self.take_tab(tab_index)?;
            return tab.panes.pop().map(|pane| (pane, carried));
        }
        let pane = w.tabs[tab_index].remove(id)?;
        // The panel browses through this pane's connection.
        if w.tabs[tab_index].files.as_ref().is_some_and(|p| p.pane == id) {
            w.tabs[tab_index].files = None;
        }
        let carried = w.carry_out(&[id]);
        if tab_index == w.active_tab && w.focused {
            send_focus(w.active_pane(), true);
        }
        w.update_title();
        w.relayout(&self.config, false);
        Some((pane, carried))
    }

    /// "Move tab to window…": a new window, or one of the others.
    pub(super) fn open_move_tab_picker(&mut self) {
        let current = self.window.as_ref().map(|w| w.id);
        let mut items = vec![PaletteItem {
            title: "New window".into(),
            detail: String::new(),
            group: String::new(),
            icon: '\u{F05B0}',
            color: None,
            action: PaletteAction::MoveTab(None),
        }];
        for (number, id) in self.window_order().into_iter().enumerate() {
            let Some(w) = self.windows().find(|w| w.id == id).filter(|w| Some(w.id) != current) else { continue };
            items.push(PaletteItem {
                title: format!("Window {}", number + 1),
                detail: w.describe(&self.config),
                group: String::new(),
                icon: '\u{F05AF}',
                color: None,
                action: PaletteAction::MoveTab(Some(id)),
            });
        }
        self.show_picker(Palette::chooser(items, "Move this tab to…", "↵ move   esc close"));
    }

    /// "Move pane to…": a tab or window of its own, or any other tab.
    pub(super) fn open_move_pane_picker(&mut self) {
        let Some(own_tab) = self.window.as_ref().map(|w| w.tabs[w.active_tab].id) else { return };
        let mut items = vec![
            PaletteItem {
                title: "New tab".into(),
                detail: String::new(),
                group: String::new(),
                icon: '\u{F0415}',
                color: None,
                action: PaletteAction::MovePane(PaneTarget::NewTab),
            },
            PaletteItem {
                title: "New window".into(),
                detail: String::new(),
                group: String::new(),
                icon: '\u{F05B0}',
                color: None,
                action: PaletteAction::MovePane(PaneTarget::NewWindow),
            },
        ];
        let order = self.window_order();
        let several = order.len() > 1;
        for (number, id) in order.into_iter().enumerate() {
            let Some(w) = self.windows().find(|w| w.id == id) else { continue };
            for (tab, info) in w.tabs.iter().zip(w.tab_infos(&self.config)) {
                if tab.id == own_tab || tab.settings.is_some() {
                    continue;
                }
                items.push(PaletteItem {
                    title: info.title,
                    detail: if several { format!("Window {}", number + 1) } else { String::new() },
                    group: "Split into".into(),
                    icon: info.icon,
                    color: info.color,
                    action: PaletteAction::MovePane(PaneTarget::Tab(tab.id)),
                });
            }
        }
        self.show_picker(Palette::chooser(items, "Move this pane to…", "↵ move   esc close"));
    }

    fn show_picker(&mut self, picker: Palette) {
        let Some(w) = &mut self.window else { return };
        w.palette = Some(picker);
        w.menu = None;
        w.window.request_redraw();
    }

    //#endregion

    //#region Dragging a tab out

    /// A tab was let go outside its window: wait a moment to see whether
    /// another window gets the pointer.
    pub(super) fn start_tab_drop(&mut self, index: usize) {
        let Some(w) = &self.window else { return };
        let Some(tab) = w.tabs.get(index) else { return };
        self.tab_drop = Some(TabDrop {
            from: w.id,
            tab: tab.id,
            until: Instant::now() + TAB_DROP_WAIT,
        });
    }

    /// When the dropped tab stops waiting for another window.
    pub(super) fn tab_drop_deadline(&self) -> Option<Instant> {
        self.tab_drop.as_ref().map(|d| d.until)
    }

    /// The pointer showed up in window `on` while a dropped tab waits: it
    /// moves there, into the tab bar slot under the pointer.
    pub(super) fn land_tab_drop(&mut self, on: WindowId, x: f64) {
        let Some(drop) = self.tab_drop.take_if(|d| d.from != on) else { return };
        if !self.select_window(on) {
            return;
        }
        let slot = self.window.as_ref().and_then(|w| {
            let infos = w.tab_infos(&self.config);
            let layout = w.bar_layout(&self.config, &infos)?;
            Some(tabbar::slot_at_x(&layout, x as f32))
        });
        if !self.select_window(drop.from) {
            return;
        }
        let Some(index) = self.window.as_ref().and_then(|w| w.tabs.iter().position(|t| t.id == drop.tab)) else { return };
        self.move_tab_to_slot(index, Some(on), slot);
    }

    /// No other window got the pointer: the tab opens a window of its own.
    pub(super) fn finish_tab_drop(&mut self, now: Instant) {
        let Some(drop) = self.tab_drop.take_if(|d| d.until <= now) else { return };
        if !self.select_window(drop.from) {
            return;
        }
        let index = self.window.as_ref().and_then(|w| w.tabs.iter().position(|t| t.id == drop.tab));
        if let Some(index) = index
            && self.window.as_ref().is_some_and(|w| w.tabs.len() > 1)
        {
            self.move_tab(index, None);
        }
    }

    //#endregion
}
