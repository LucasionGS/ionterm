//! The app's side of remote files: opening the panel, carrying out what it
//! asks for, transfers and their name conflicts, and the dialogs, toasts and
//! transfer card that go with them.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ionterm_render::{CellMetrics, ChromeItem, text_columns};
use winit::window::CursorIcon;

use super::{App, TermWindow};
use crate::config::Config;
use crate::dialog::{Answer, Button, Dialog};
use crate::files::panel::{self, FilePanel, Request, Target, TransferLayout};
use crate::files::remote::{self, Entry, Remote};
use crate::files::transfer::{self, Direction, Transfer, TransferEvent, TransferId, TransferSpec};
use crate::files::{self, FilesEvent};
use crate::input::MouseButton;
use crate::layout::{PaneId, Rect};
use crate::tabbar::{mix, truncate};
use crate::theme::Theme;

/// How long a toast stays up.
const TOAST_TIME: Duration = Duration::from_secs(4);
/// How long a panel's status message stays up.
const STATUS_TIME: Duration = Duration::from_secs(4);
/// Transfers shown in the floating card at most.
const CARD_TRANSFERS: usize = 3;

/// What answering the open dialog does.
pub enum Purpose {
    Delete { pane: PaneId, paths: Vec<String> },
    Rename { pane: PaneId, dir: String, from: String },
    NewFolder { pane: PaneId, dir: String },
    Conflict(Conflicts),
    /// Multi-line text waiting to be pasted into a pane.
    Paste { pane: PaneId, text: String },
}

/// Transfers whose destination name is taken, asked about one at a time.
pub struct Conflicts {
    pane: PaneId,
    current: TransferSpec,
    rest: VecDeque<TransferSpec>,
    /// Names taken in an upload's remote folder, so renamed copies get a
    /// free one. Downloads check the local folder directly.
    taken: HashSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Choice {
    Overwrite,
    Rename,
    Skip,
}

impl TermWindow {
    /// The tab whose file panel browses through `pane`.
    fn panel_tab(&self, pane: PaneId) -> Option<usize> {
        self.tabs.iter().position(|t| t.files.as_ref().is_some_and(|p| p.pane == pane))
    }

    fn panel_for(&mut self, pane: PaneId) -> Option<&mut FilePanel> {
        self.tabs.iter_mut().find_map(|t| t.files.as_mut().filter(|p| p.pane == pane))
    }

    /// Whether the active tab's file panel has the keyboard.
    pub(super) fn panel_focused(&self) -> bool {
        self.tabs[self.active_tab].files.as_ref().is_some_and(|p| p.focused)
    }

    pub(super) fn show_toast(&mut self, message: impl Into<String>) {
        self.toast = Some((message.into(), Instant::now()));
        self.window.request_redraw();
    }

    /// Transfers shown in the floating card: those whose panel isn't on
    /// screen.
    fn card_transfers(&self) -> Vec<&Transfer> {
        let shown = self.tabs[self.active_tab].files.as_ref().map(|p| p.pane);
        self.transfers.list.iter().filter(|t| Some(t.pane) != shown).collect()
    }

    /// The floating transfer card in the bottom-right corner of the panes.
    fn card_layout(&self, area: Rect) -> Option<(Rect, Vec<TransferLayout>, usize)> {
        let transfers = self.card_transfers();
        if transfers.is_empty() {
            return None;
        }
        let m = self.renderer.cell_metrics();
        let scale = self.scale_factor as f32;
        let px = |v: f32| (v * scale).round();
        let area = self.tabs[self.active_tab].pane_area(area);
        let width = (m.width * 44.0).min(area.width - px(24.0)).max(m.width * 20.0);
        let x = area.x + area.width - width - px(12.0);
        let pad = px(6.0);
        let bottom = area.y + area.height - px(12.0) - pad;
        let (layouts, more, top) = panel::layout_transfers(&transfers, x + pad, width - 2.0 * pad, bottom, CARD_TRANSFERS, m, scale);
        let rect = Rect::new(x, top - pad, width, bottom - top + 2.0 * pad + if more > 0 { m.height } else { 0.0 });
        Some((rect, layouts, more))
    }

    /// The panes' area below or above the tab bar.
    pub(super) fn content_area(&self) -> Rect {
        let size = self.window.inner_size();
        let mut area = Rect::new(0.0, 0.0, size.width as f32, size.height as f32);
        if let Some(bar) = self.tab_bar {
            area.height -= bar.height;
            if bar.y == 0.0 {
                area.y = bar.height;
            }
        }
        area
    }

    /// Draws the active tab's file panel, the transfer card, and the toast.
    pub(super) fn draw_files(&mut self, theme: &Theme, out: &mut Vec<ChromeItem>) {
        let m = self.renderer.cell_metrics();
        let scale = self.scale_factor as f32;
        let tab = self.active_tab;
        if let Some(panel) = &self.tabs[tab].files {
            let transfers: Vec<&Transfer> = self.transfers.for_pane(panel.pane).collect();
            let layout = panel::layout(panel, &transfers, panel.rect, m, scale);
            panel::draw(panel, &layout, &transfers, theme, m, scale, out);
            let page = ((layout.list.height / layout.row_height).floor() as usize).max(1);
            if let Some(panel) = &mut self.tabs[tab].files {
                panel.page = page;
            }
        }
        let area = self.content_area();
        let card = self.card_layout(area);
        let card_rect = card.as_ref().map(|(rect, _, _)| *rect);
        if let Some((rect, layouts, more)) = card {
            let bg = theme.background.0;
            let fg = theme.foreground.0;
            let card_bg = mix(bg, fg, 0.08);
            let edge = scale.round().max(1.0);
            let radius = (8.0 * scale).round();
            out.push(ChromeItem::RoundedRect {
                rect: [rect.x - edge, rect.y - edge, rect.width + 2.0 * edge, rect.height + 2.0 * edge],
                radius: radius + edge,
                top_only: false,
                color: mix(bg, fg, 0.2),
                alpha: 1.0,
            });
            out.push(ChromeItem::RoundedRect {
                rect: [rect.x, rect.y, rect.width, rect.height],
                radius,
                top_only: false,
                color: card_bg,
                alpha: 1.0,
            });
            let transfers = self.card_transfers();
            panel::draw_transfers(&transfers, &layouts, self.card_hover.as_ref(), card_bg, theme, m, scale, out);
            if more > 0 {
                let y = layouts.last().map_or(rect.y, |l| l.rect.y + l.rect.height);
                out.push(ChromeItem::Text {
                    text: format!("+{more} more"),
                    x: rect.x + (8.0 * scale).round(),
                    y,
                    color: mix(fg, card_bg, 0.45),
                    bold: false,
                });
            }
        }
        if let Some((message, _)) = &self.toast {
            draw_toast(message, area, card_rect, theme, m, scale, out);
        }
    }

    /// Updates hover for the panel and the transfer card. Returns whether
    /// the pointer is over either (so the panes don't get it).
    pub(super) fn files_hover(&mut self, x: f32, y: f32) -> bool {
        let m = self.renderer.cell_metrics();
        let scale = self.scale_factor as f32;
        let area = self.content_area();
        let card = self
            .card_layout(area)
            .filter(|(rect, _, _)| rect.contains(x, y))
            .map(|(_, layouts, _)| card_target(&layouts, x, y));
        let card_hover = card.clone().flatten();
        if card_hover != self.card_hover {
            self.card_hover = card_hover;
            self.window.request_redraw();
        }
        if let Some(target) = card {
            self.set_cursor_icon(if target.is_some() { CursorIcon::Pointer } else { CursorIcon::Default });
            return true;
        }

        let tab = self.active_tab;
        let Some(panel) = &self.tabs[tab].files else { return false };
        let transfers: Vec<&Transfer> = self.transfers.for_pane(panel.pane).collect();
        let layout = panel::layout(panel, &transfers, panel.rect, m, scale);
        let hit = panel::hit(&layout, x, y);
        drop(transfers);
        let inside = hit.is_some();
        let hover = hit.filter(|t| *t != Target::Empty);
        let icon = match &hover {
            Some(Target::Row(_)) | None => CursorIcon::Default,
            Some(_) => CursorIcon::Pointer,
        };
        if let Some(panel) = &mut self.tabs[tab].files
            && panel.hover != hover
        {
            panel.hover = hover;
            self.window.request_redraw();
        }
        if inside {
            self.set_cursor_icon(icon);
        }
        inside
    }

    /// Scrolls the panel when the wheel turns over it. Returns whether it did.
    pub(super) fn files_wheel(&mut self, x: f32, y: f32, lines: isize) -> bool {
        let tab = self.active_tab;
        let Some(panel) = &mut self.tabs[tab].files else { return false };
        if !panel.rect.contains(x, y) {
            return false;
        }
        panel.scroll_by(-lines * 3);
        self.window.request_redraw();
        true
    }
}

/// The transfer button under a point in the card.
fn card_target(layouts: &[TransferLayout], x: f32, y: f32) -> Option<Target> {
    layouts.iter().find_map(|t| {
        if t.close.contains(x, y) {
            Some(Target::Transfer(t.id, panel::TransferPart::Close))
        } else if t.reveal.is_some_and(|r| r.contains(x, y)) {
            Some(Target::Transfer(t.id, panel::TransferPart::Reveal))
        } else {
            None
        }
    })
}

/// A message centred at the bottom, above the transfer card if they'd meet.
fn draw_toast(message: &str, area: Rect, card: Option<Rect>, theme: &Theme, m: CellMetrics, scale: f32, out: &mut Vec<ChromeItem>) {
    let bg = theme.background.0;
    let fg = theme.foreground.0;
    let toast_bg = mix(bg, fg, 0.12);
    let max_cols = ((area.width / m.width) as usize).saturating_sub(6).max(8);
    let text = truncate(message, max_cols);
    let width = (text_columns(&text) as f32 + 3.0) * m.width;
    let height = (m.height * 1.9).round();
    let x = (area.x + (area.width - width) / 2.0).round();
    let gap = (16.0 * scale).round();
    let mut y = (area.y + area.height - height - gap).round();
    if let Some(card) = card
        && x + width > card.x
        && x < card.x + card.width
    {
        y = y.min(card.y - height - (8.0 * scale).round());
    }
    out.push(ChromeItem::RoundedRect {
        rect: [x, y, width, height],
        radius: (height / 2.0).round(),
        top_only: false,
        color: toast_bg,
        alpha: 1.0,
    });
    out.push(ChromeItem::Text {
        text,
        x: x + 1.5 * m.width,
        y: y + ((height - m.height) / 2.0).round(),
        color: fg,
        bold: false,
    });
}

impl App {
    /// Opens the active tab's file panel, focuses it, or closes it when it
    /// already has the keyboard.
    pub(super) fn toggle_files(&mut self) {
        let Some(w) = &mut self.window else { return };
        let tab = w.active_tab;
        if let Some(panel) = &mut w.tabs[tab].files {
            if panel.focused {
                w.tabs[tab].files = None;
                w.relayout(&self.config, false);
            } else {
                panel.focused = true;
                w.window.request_redraw();
            }
            return;
        }
        self.open_files(tab);
    }

    /// Opens a tab's file panel on its active pane's connection, in the
    /// shell's directory when known.
    pub(super) fn open_files(&mut self, tab: usize) {
        let show_hidden = self.config.files.show_hidden;
        let Some(w) = &mut self.window else { return };
        let Some(t) = w.tabs.get(tab) else { return };
        if t.files.is_some() {
            if let Some(panel) = &mut w.tabs[tab].files {
                panel.focused = true;
            }
            w.window.request_redraw();
            return;
        }
        let pane = t.active_pane();
        let remote = match &pane.connection {
            Some(connection) if connection.managed => Remote::new(connection),
            Some(_) => {
                w.show_toast("Files need a connection opened by ionterm: open this host from ctrl+shift+s.");
                return;
            }
            None if pane.detected_ssh.is_some() => {
                w.show_toast("Files need a connection opened by ionterm: open this host from ctrl+shift+s.");
                return;
            }
            None => {
                w.show_toast("The file panel is for SSH tabs: connect with ctrl+shift+s.");
                return;
            }
        };
        let start = pane.remote_cwd.clone();
        let panel = FilePanel::new(pane.id, remote, start.clone(), show_hidden);
        w.tabs[tab].files = Some(panel);
        w.relayout(&self.config, false);
        self.list_panel(tab, start);
    }

    /// Starts listing a folder for a tab's panel.
    fn list_panel(&mut self, tab: usize, path: Option<String>) {
        let proxy = self.proxy.clone();
        let Some(w) = &mut self.window else { return };
        let Some(panel) = w.tabs.get_mut(tab).and_then(|t| t.files.as_mut()) else { return };
        panel.request += 1;
        panel.loading = true;
        if path.is_some() {
            panel.path = path.clone();
        }
        files::list(proxy, panel.pane, panel.request, panel.remote.clone(), path);
        w.window.request_redraw();
    }

    /// Carries out what a tab's panel asked for.
    pub(super) fn panel_request(&mut self, tab: usize, request: Request) {
        let proxy = self.proxy.clone();
        let Some(w) = &mut self.window else { return };
        let Some(panel) = w.tabs.get_mut(tab).and_then(|t| t.files.as_mut()) else { return };
        let (pane, remote) = (panel.pane, panel.remote.clone());
        let dir = panel.dir().map(String::from);
        w.window.request_redraw();
        match request {
            Request::List(path) => self.list_panel(tab, path),
            Request::Download(entries) => {
                if let Some(dir) = dir {
                    self.queue_downloads(pane, remote, &dir, entries);
                }
            }
            Request::Upload { folders } => {
                if let Some(dir) = dir {
                    files::pick(proxy, pane, dir, folders);
                }
            }
            Request::Rename(entry) => {
                let Some(dir) = dir else { return };
                let kind = if entry.is_dir() { "folder" } else { "file" };
                w.dialog = Some(
                    Dialog::new(
                        format!("Rename {kind}"),
                        format!("New name for “{}”:", entry.name),
                        vec![Button::new("Rename", 'r'), Button::new("Cancel", 'c')],
                        Purpose::Rename {
                            pane,
                            dir,
                            from: entry.name.clone(),
                        },
                    )
                    .with_input(entry.name),
                );
            }
            Request::Delete(entries) => {
                let Some(dir) = dir else { return };
                if entries.is_empty() {
                    return;
                }
                let message = match entries.as_slice() {
                    [entry] if entry.is_dir() => {
                        format!("Delete the folder “{}” and everything in it from {}? This can't be undone.", entry.name, remote.alias)
                    }
                    [entry] => format!("Delete “{}” from {}? This can't be undone.", entry.name, remote.alias),
                    many => format!("Delete {} items from {}? This can't be undone.", many.len(), remote.alias),
                };
                let paths = entries.iter().map(|e| remote::join(&dir, &e.name)).collect();
                w.dialog = Some(Dialog::new(
                    "Delete?",
                    message,
                    vec![Button::new("Delete", 'd').danger(), Button::new("Cancel", 'c')],
                    Purpose::Delete { pane, paths },
                ));
                if let Some(d) = &mut w.dialog {
                    // Cancel is the safe default.
                    d.default = 1;
                }
            }
            Request::NewFolder => {
                let Some(dir) = dir else { return };
                w.dialog = Some(
                    Dialog::new(
                        "New folder",
                        format!("Create a folder in {dir}:"),
                        vec![Button::new("Create", 'c'), Button::new("Cancel", 'x')],
                        Purpose::NewFolder { pane, dir },
                    )
                    .with_input(""),
                );
            }
            Request::CopyPath(path) => {
                if let Some(panel) = w.tabs.get_mut(tab).and_then(|t| t.files.as_mut()) {
                    panel.set_status(format!("Copied {path}"));
                }
                self.set_clipboard(path, false);
            }
            Request::Cancel(id) => self.cancel_or_dismiss(id),
            Request::Reveal(id) => {
                if let Some(t) = w.transfers.list.iter().find(|t| t.id == id) {
                    files::reveal(&t.local_path);
                }
            }
            Request::Close => {
                w.tabs[tab].files = None;
                w.relayout(&self.config, false);
            }
            Request::Unfocus => {
                if let Some(panel) = &mut w.tabs[tab].files {
                    panel.focused = false;
                }
            }
            Request::Follow => self.follow_cwd(tab, pane),
        }
    }

    fn cancel_or_dismiss(&mut self, id: TransferId) {
        let Some(w) = &mut self.window else { return };
        match w.transfers.list.iter().find(|t| t.id == id) {
            Some(t) if t.is_active() => w.transfers.cancel(id),
            Some(_) => w.transfers.dismiss(id),
            None => {}
        }
        w.window.request_redraw();
    }

    //#region Transfers

    /// Downloads entries of a remote folder, asking about names that are
    /// already taken locally.
    fn queue_downloads(&mut self, pane: PaneId, remote: Remote, dir: &str, entries: Vec<Entry>) {
        let target_dir = transfer::download_dir(&self.config.files.download_dir);
        let specs = entries
            .into_iter()
            .map(|entry| TransferSpec {
                direction: Direction::Download,
                remote: remote.clone(),
                remote_path: remote::join(dir, &entry.name),
                local_path: target_dir.join(transfer::local_name(&entry.name)),
                is_dir: entry.is_dir(),
                merge: false,
            })
            .collect();
        self.start_transfers(pane, specs, HashSet::new());
    }

    /// Downloads one remote path, for `ionterm-get`.
    fn download_path(&mut self, pane: PaneId, remote: Remote, path: &str, is_dir: bool) {
        let (dir, name) = match path.trim_end_matches('/').rsplit_once('/') {
            Some(("", name)) => ("/".to_string(), name.to_string()),
            Some((dir, name)) => (dir.to_string(), name.to_string()),
            None => return,
        };
        if name.is_empty() {
            return;
        }
        let entry = Entry {
            name,
            kind: if is_dir { remote::EntryKind::Dir } else { remote::EntryKind::File },
            size: 0,
            mtime: None,
            mode: String::new(),
            owner: String::new(),
            target: None,
        };
        self.queue_downloads(pane, remote, &dir, vec![entry]);
    }

    /// `ionterm-get` or `ionterm-put` in a pane's remote shell. Only
    /// connections ionterm opened have these commands, and only their
    /// requests are followed.
    pub(super) fn file_request(&mut self, pane: PaneId, request: ionterm_vt::FileRequest) {
        let proxy = self.proxy.clone();
        let Some(w) = &mut self.window else { return };
        let Some(connection) = w
            .tabs
            .iter()
            .flat_map(|t| &t.panes)
            .find(|p| p.id == pane)
            .and_then(|p| p.connection.as_ref())
            .filter(|c| c.managed)
        else {
            return;
        };
        let remote = Remote::new(connection);
        match request {
            ionterm_vt::FileRequest::Get { path, dir } => {
                let name = path.rsplit('/').next().unwrap_or_default().to_string();
                w.show_toast(format!("Downloading {name}"));
                self.download_path(pane, remote, &path, dir);
            }
            ionterm_vt::FileRequest::Put { dir } => files::pick(proxy, pane, dir, false),
        }
    }

    /// Uploads local files and folders into a remote folder (`None` = the
    /// remote home), after looking up which names are already taken there.
    pub(super) fn upload(&mut self, pane: PaneId, remote: Remote, dir: Option<String>, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        files::check_upload(self.proxy.clone(), pane, remote, dir, paths);
    }

    /// Starts transfers whose destination is free; asks about the rest.
    fn start_transfers(&mut self, pane: PaneId, specs: Vec<TransferSpec>, taken: HashSet<String>) {
        let Some(w) = &mut self.window else { return };
        let mut conflicts = VecDeque::new();
        for spec in specs {
            let exists = match spec.direction {
                Direction::Download => std::fs::symlink_metadata(&spec.local_path).is_ok(),
                Direction::Upload => taken.contains(&spec.name()),
            };
            if exists {
                conflicts.push_back(spec);
            } else {
                w.transfers.add(pane, spec);
            }
        }
        w.window.request_redraw();
        if let Some(current) = conflicts.pop_front() {
            let queue = Conflicts {
                pane,
                current,
                rest: conflicts,
                taken,
            };
            self.ask_conflict(queue);
        }
    }

    /// Opens the dialog for the first transfer of a conflict queue.
    fn ask_conflict(&mut self, queue: Conflicts) {
        let Some(w) = &mut self.window else { return };
        let spec = &queue.current;
        let name = spec.name();
        let kind = if spec.is_dir { "folder" } else { "file" };
        let (title, mut message) = match spec.direction {
            Direction::Download => {
                let dir = spec.local_path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                (format!("Replace “{name}”?"), format!("A {kind} named “{name}” is already in {dir}."))
            }
            Direction::Upload => {
                let dir = remote::parent(&spec.remote_path);
                (
                    format!("Replace “{name}” on {}?", spec.remote.alias),
                    format!("A {kind} named “{name}” is already in {dir}."),
                )
            }
        };
        if spec.is_dir {
            message.push_str(" Overwriting merges the folders, replacing files with the same name.");
        }
        let others = queue.rest.len();
        let mut dialog = Dialog::new(
            title,
            message,
            vec![Button::new("Overwrite", 'o').danger(), Button::new("Rename", 'r'), Button::new("Skip", 's')],
            Purpose::Conflict(queue),
        );
        // Keeping both is the safe default.
        dialog.default = 1;
        if others > 0 {
            dialog = dialog.with_checkbox(format!("Do this for the other {others}"));
        }
        w.dialog = Some(dialog);
        w.window.request_redraw();
    }

    /// Applies a conflict answer to one transfer and starts it.
    fn resolve(&mut self, pane: PaneId, mut spec: TransferSpec, choice: Choice, taken: &mut HashSet<String>) {
        match choice {
            Choice::Skip => return,
            Choice::Overwrite => spec.merge = spec.is_dir,
            Choice::Rename => match spec.direction {
                Direction::Download => {
                    let dir = spec.local_path.parent().map(PathBuf::from).unwrap_or_default();
                    let name = spec.local_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    spec.local_path = transfer::free_local_path(&dir, &name);
                }
                Direction::Upload => {
                    let dir = remote::parent(&spec.remote_path);
                    let name = spec.name();
                    let free = (1..)
                        .map(|n| transfer::numbered_name(&name, n))
                        .find(|candidate| !taken.contains(candidate))
                        .expect("some number is free");
                    taken.insert(free.clone());
                    spec.remote_path = remote::join(&dir, &free);
                }
            },
        }
        if let Some(w) = &mut self.window {
            w.transfers.add(pane, spec);
        }
    }

    pub(super) fn transfer_event(&mut self, id: TransferId, event: TransferEvent) {
        let Some(w) = &mut self.window else { return };
        w.window.request_redraw();
        let Some(finished) = w.transfers.event(id, event) else { return };
        let Some(tab) = w.panel_tab(finished.pane) else { return };
        let Some(panel) = w.tabs[tab].files.as_mut() else { return };
        let name = finished
            .local_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match (finished.direction, finished.ok) {
            (Direction::Download, true) => {
                let dir = finished.local_path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                panel.set_status(format!("Saved {name} to {dir}"));
            }
            (Direction::Upload, true) => {
                let refresh = panel.dir() == Some(remote::parent(&finished.remote_path).as_str());
                panel.set_status(format!("Uploaded {name}"));
                if refresh {
                    let request = panel.refresh();
                    self.panel_request(tab, request);
                }
            }
            (_, false) => {}
        }
    }

    //#endregion

    /// Results of background work for the panels.
    pub(super) fn files_event(&mut self, event: FilesEvent) {
        let Some(w) = &mut self.window else { return };
        w.window.request_redraw();
        match event {
            FilesEvent::Listed { pane, request, result } => {
                let Some(panel) = w.panel_for(pane) else { return };
                if panel.request != request {
                    return;
                }
                match result {
                    Ok(listing) => panel.set_listing(listing),
                    Err(error) => {
                        panel.loading = false;
                        panel.reselect = None;
                        if panel.listing.is_some() {
                            // Stay where we were; say why we couldn't go.
                            panel.path = panel.dir().map(String::from);
                            panel.set_status(error);
                        } else {
                            panel.error = Some(error);
                        }
                    }
                }
            }
            FilesEvent::Changed { pane, result, select } => {
                let Some(tab) = w.panel_tab(pane) else { return };
                let Some(panel) = w.tabs[tab].files.as_mut() else { return };
                match result {
                    Ok(message) => {
                        panel.set_status(message);
                        panel.reselect = select;
                        let request = panel.refresh();
                        self.panel_request(tab, request);
                    }
                    Err(error) => panel.set_status(error),
                }
            }
            FilesEvent::UploadTargets { pane, dir, paths, existing } => {
                let taken = match existing {
                    Ok(taken) => taken,
                    Err(error) => {
                        let message = format!("Can't upload to {dir}: {error}");
                        match w.panel_for(pane) {
                            Some(panel) => panel.set_status(message),
                            None => w.show_toast(message),
                        }
                        return;
                    }
                };
                let Some(remote) = w.panel_tab(pane).and_then(|t| w.tabs[t].files.as_ref()).map(|p| p.remote.clone()).or_else(|| {
                    w.tabs.iter().flat_map(|t| &t.panes).find(|p| p.id == pane).and_then(|p| p.connection.as_ref()).map(Remote::new)
                }) else {
                    return;
                };
                let specs = paths
                    .into_iter()
                    .filter_map(|path| {
                        let name = path.file_name()?.to_string_lossy().into_owned();
                        Some(TransferSpec {
                            direction: Direction::Upload,
                            remote: remote.clone(),
                            remote_path: remote::join(&dir, &name),
                            is_dir: path.is_dir(),
                            local_path: path,
                            merge: false,
                        })
                    })
                    .collect();
                self.start_transfers(pane, specs, taken);
            }
            FilesEvent::Picked { pane, dir, paths } => {
                // From the panel, or from `ionterm-put` with no panel open.
                let remote = w.panel_for(pane).map(|p| p.remote.clone()).or_else(|| {
                    w.tabs
                        .iter()
                        .flat_map(|t| &t.panes)
                        .find(|p| p.id == pane)
                        .and_then(|p| p.connection.as_ref())
                        .filter(|c| c.managed)
                        .map(Remote::new)
                });
                let Some(remote) = remote else { return };
                self.upload(pane, remote, Some(dir), paths);
            }
            FilesEvent::ZmodemPicked { pane, paths } => self.zmodem_picked(pane, paths),
        }
    }

    /// Acts on the dialog's answer.
    pub(super) fn dialog_answer(&mut self, answer: Answer) {
        let proxy = self.proxy.clone();
        let Some(w) = &mut self.window else { return };
        w.window.request_redraw();
        let Some(dialog) = w.dialog.take() else { return };
        let remote_of = |w: &mut TermWindow, pane: PaneId| w.panel_for(pane).map(|p| p.remote.clone());
        match dialog.purpose {
            Purpose::Delete { pane, paths } => {
                if answer.button != Some(0) {
                    return;
                }
                let Some(remote) = remote_of(w, pane) else { return };
                let count = paths.len();
                files::change(proxy, pane, None, move || {
                    remote.delete(&paths).map(|()| {
                        if count == 1 {
                            format!("Deleted {}", paths[0].rsplit('/').next().unwrap_or_default())
                        } else {
                            format!("Deleted {count} items")
                        }
                    })
                });
            }
            Purpose::Rename { pane, ref dir, ref from } => {
                if answer.button != Some(0) {
                    return;
                }
                let name = answer.text.trim().to_string();
                if name == *from || name.is_empty() {
                    return;
                }
                if let Some(problem) = bad_name(&name) {
                    w.dialog = Some(Dialog { error: Some(problem), ..dialog });
                    return;
                }
                let Some(remote) = remote_of(w, pane) else { return };
                let (old, new) = (remote::join(dir, from), remote::join(dir, &name));
                let message = format!("Renamed to {name}");
                files::change(proxy, pane, Some(name), move || remote.rename(&old, &new).map(|()| message));
            }
            Purpose::NewFolder { pane, ref dir } => {
                if answer.button != Some(0) {
                    return;
                }
                let name = answer.text.trim().to_string();
                if name.is_empty() {
                    return;
                }
                if let Some(problem) = bad_name(&name) {
                    w.dialog = Some(Dialog { error: Some(problem), ..dialog });
                    return;
                }
                let Some(remote) = remote_of(w, pane) else { return };
                let path = remote::join(dir, &name);
                let message = format!("Created {name}");
                files::change(proxy, pane, Some(name), move || remote.mkdir(&path).map(|()| message));
            }
            Purpose::Paste { pane, text } => {
                let Some(p) = w.tabs.iter().flat_map(|t| &t.panes).find(|p| p.id == pane) else { return };
                let text = match answer.button {
                    Some(0) => text,
                    // Joined into one line, nothing runs by itself.
                    Some(1) => text.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" "),
                    _ => return,
                };
                let mode = p.lock().mode();
                p.write(&crate::input::encode_paste(&text, mode));
                p.lock().scroll_to_bottom();
            }
            Purpose::Conflict(mut queue) => {
                let choice = match answer.button {
                    Some(0) => Choice::Overwrite,
                    Some(1) => Choice::Rename,
                    // Skip, or dismissed: dismissing skips the rest too.
                    Some(_) => Choice::Skip,
                    None => return,
                };
                let pane = queue.pane;
                self.resolve(pane, queue.current, choice, &mut queue.taken);
                if answer.checked {
                    for spec in queue.rest.drain(..) {
                        self.resolve(pane, spec, choice, &mut queue.taken);
                    }
                }
                if let Some(current) = queue.rest.pop_front() {
                    queue.current = current;
                    self.ask_conflict(queue);
                }
            }
        }
    }

    /// Handles a mouse button over the panel or the transfer card. Returns
    /// whether it was theirs.
    pub(super) fn files_click(&mut self, button: MouseButton, pressed: bool) -> bool {
        let Some(w) = &mut self.window else { return false };
        // A drag that started in a pane stays with the pane.
        if w.capture.is_some() {
            return false;
        }
        let (x, y) = (w.pointer.x as f32, w.pointer.y as f32);
        let m = w.renderer.cell_metrics();
        let scale = w.scale_factor as f32;
        let area = w.content_area();
        if let Some((rect, layouts, _)) = w.card_layout(area)
            && rect.contains(x, y)
        {
            if pressed && button == MouseButton::Left {
                match card_target(&layouts, x, y) {
                    Some(Target::Transfer(id, panel::TransferPart::Close)) => self.cancel_or_dismiss(id),
                    Some(Target::Transfer(id, panel::TransferPart::Reveal)) => {
                        if let Some(t) = w.transfers.list.iter().find(|t| t.id == id) {
                            files::reveal(&t.local_path);
                        }
                    }
                    _ => {}
                }
            }
            return true;
        }

        let tab = w.active_tab;
        let Some(panel) = &mut w.tabs[tab].files else { return false };
        let transfers: Vec<&Transfer> = w.transfers.for_pane(panel.pane).collect();
        let layout = panel::layout(panel, &transfers, panel.rect, m, scale);
        let hit = panel::hit(&layout, x, y);
        drop(transfers);
        let Some(target) = hit else {
            // A click elsewhere closes the row menu and hands the keyboard back.
            if pressed {
                panel.menu = None;
                if panel.focused {
                    panel.focused = false;
                    w.window.request_redraw();
                }
            }
            return false;
        };
        if !pressed {
            return true;
        }
        panel.focused = true;
        w.window.request_redraw();
        let request = match button {
            MouseButton::Left => panel.click(&target, false, x, y),
            MouseButton::Right => panel.click(&target, true, x, y),
            _ => None,
        };
        if let Some(request) = request {
            self.panel_request(tab, request);
        }
        true
    }

    /// Feeds a key to the focused panel.
    pub(super) fn files_key(&mut self, event: &winit::event::KeyEvent) {
        let Some(w) = &mut self.window else { return };
        let tab = w.active_tab;
        let (ctrl, shift) = (w.modifiers.control_key(), w.modifiers.shift_key());
        let Some(panel) = &mut w.tabs[tab].files else { return };
        w.window.request_redraw();
        if let Some(request) = panel.key(&event.logical_key, event.text.as_deref(), ctrl, shift) {
            self.panel_request(tab, request);
        }
    }

    /// The remote shell reported its directory (after every command, or
    /// following was just turned on): a following panel shows it, even
    /// when the panel was browsed elsewhere meanwhile.
    pub(super) fn follow_cwd(&mut self, tab: usize, pane: PaneId) {
        let Some(w) = &self.window else { return };
        let Some(t) = w.tabs.get(tab) else { return };
        let Some(panel) = t.files.as_ref().filter(|p| p.pane == pane && p.follow) else { return };
        let Some(cwd) = t.pane(pane).and_then(|p| p.remote_cwd.clone()) else { return };
        if panel.dir() == Some(cwd.as_str()) || (panel.loading && panel.path.as_deref() == Some(cwd.as_str())) {
            return;
        }
        self.list_panel(tab, Some(cwd));
    }

    /// Drops expired toasts, statuses and finished transfers. Returns when
    /// to check again.
    pub(super) fn expire_files(&mut self, now: Instant) -> Option<Instant> {
        let w = self.window.as_mut()?;
        let (changed, mut next) = w.transfers.expire(now);
        let mut redraw = changed;
        if let Some((_, at)) = &w.toast {
            if now.duration_since(*at) >= TOAST_TIME {
                w.toast = None;
                redraw = true;
            } else {
                next = [next, Some(*at + TOAST_TIME)].into_iter().flatten().min();
            }
        }
        if let Some(panel) = &mut w.tabs[w.active_tab].files
            && let Some((_, at)) = &panel.status
        {
            if now.duration_since(*at) >= STATUS_TIME {
                panel.status = None;
                redraw = true;
            } else {
                next = [next, Some(*at + STATUS_TIME)].into_iter().flatten().min();
            }
        }
        if redraw {
            w.window.request_redraw();
        }
        next
    }
}

/// Why a name can't be used for a remote file, if it can't.
fn bad_name(name: &str) -> Option<String> {
    if name.contains('/') {
        Some("Names can't contain “/”.".into())
    } else if name == "." || name == ".." {
        Some("That name is reserved.".into())
    } else if name.contains('\0') {
        Some("Names can't contain NUL characters.".into())
    } else {
        None
    }
}

/// Places every tab's file panel on the right of `area`.
pub(super) fn place_panels(tabs: &mut [crate::tab::Tab], area: Rect, config: &Config, m: CellMetrics) {
    for tab in tabs {
        if let Some(panel) = &mut tab.files {
            let width = panel::width(config.files.panel_width, m, area);
            panel.rect = Rect::new(area.x + area.width - width, area.y, width, area.height);
        }
    }
}
