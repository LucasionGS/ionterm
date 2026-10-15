//! The file panel: a remote file browser docked on the right of an SSH tab,
//! with a strip of transfers at its bottom.
//!
//! Like the other widgets it has `layout()` (pure geometry), `hit()` and
//! `draw()`; what it asks the app to do comes back as a `Request`.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ionterm_render::{CellMetrics, ChromeItem, text_columns};
use ionterm_vt::Rgb;
use winit::keyboard::{Key, NamedKey};

use crate::files::remote::{self, Entry, EntryKind, Listing, Remote};
use crate::files::transfer::{Direction, Transfer, TransferId, TransferState};
use crate::layout::{PaneId, Rect};
use crate::tabbar::{mix, truncate};
use crate::theme::Theme;

const MAX_TRANSFERS_SHOWN: usize = 4;
/// Multi-click window for opening rows.
const DOUBLE_CLICK_MS: u128 = 400;

/// A row of the list: the parent folder, or an entry of the listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    Up,
    Entry(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Up,
    Refresh,
    NewFolder,
    UploadFiles,
    UploadFolder,
    Hidden,
    Follow,
    Close,
}

impl Button {
    const ALL: [Button; 8] = [
        Button::Follow,
        Button::Hidden,
        Button::Up,
        Button::Refresh,
        Button::NewFolder,
        Button::UploadFiles,
        Button::UploadFolder,
        Button::Close,
    ];

    fn icon(self, panel: &FilePanel) -> char {
        match self {
            Button::Up => '\u{F062}',
            Button::Refresh => '\u{F021}',
            Button::NewFolder => '\u{F0257}',
            Button::UploadFiles => '\u{F093}',
            Button::UploadFolder => '\u{F0259}',
            Button::Hidden if panel.show_hidden => '\u{F06E}',
            Button::Hidden => '\u{F070}',
            Button::Follow => '\u{F120}',
            Button::Close => '\u{F00D}',
        }
    }

    /// Shown in the status line while hovered.
    pub fn tooltip(self) -> &'static str {
        match self {
            Button::Up => "Parent folder (Backspace)",
            Button::Refresh => "Refresh (F5)",
            Button::NewFolder => "New folder (ctrl+n)",
            Button::UploadFiles => "Upload files (ctrl+u)",
            Button::UploadFolder => "Upload a folder",
            Button::Hidden => "Show hidden files (ctrl+h)",
            Button::Follow => "Follow the terminal's directory",
            Button::Close => "Close the panel (ctrl+shift+o)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuItem {
    Open,
    Download,
    Rename,
    Delete,
    CopyPath,
}

impl MenuItem {
    fn label(self) -> &'static str {
        match self {
            MenuItem::Open => "Open",
            MenuItem::Download => "Download",
            MenuItem::Rename => "Rename…  F2",
            MenuItem::Delete => "Delete…  Del",
            MenuItem::CopyPath => "Copy path",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferPart {
    /// Cancel a running transfer, or dismiss a finished one.
    Close,
    /// Show a finished download in the file manager.
    Reveal,
}

/// Something in the panel that can be hovered or clicked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Row(usize),
    Button(Button),
    /// A breadcrumb: the folder it leads to.
    Crumb(String),
    Transfer(TransferId, TransferPart),
    Menu(MenuItem),
    /// Inside the panel, on nothing in particular.
    Empty,
}

/// Work the panel asks the app to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// List this folder (`None` = home).
    List(Option<String>),
    Download(Vec<Entry>),
    Upload { folders: bool },
    Rename(Entry),
    Delete(Vec<Entry>),
    NewFolder,
    CopyPath(String),
    /// Cancel a running transfer, or dismiss a finished one.
    Cancel(TransferId),
    Reveal(TransferId),
    Close,
    /// Give the keyboard back to the terminal.
    Unfocus,
    /// Following was turned on: show the terminal's directory now.
    Follow,
}

/// The right-click menu on a row.
pub struct RowMenu {
    pub row: usize,
    pub x: f32,
    pub y: f32,
}

pub struct FilePanel {
    /// The pane whose connection this browses.
    pub pane: PaneId,
    pub remote: Remote,
    /// The folder shown, or being opened.
    pub path: Option<String>,
    pub listing: Option<Listing>,
    /// A listing is on its way.
    pub loading: bool,
    /// Why the last listing failed.
    pub error: Option<String>,
    /// The latest listing request; older answers are ignored.
    pub request: u64,
    /// Position among the visible rows.
    pub selected: usize,
    /// First visible row shown.
    pub scroll: usize,
    /// Typed text narrowing the list.
    pub filter: String,
    pub show_hidden: bool,
    /// Follow the shell's directory (OSC 7) as it changes.
    pub follow: bool,
    /// Has the keyboard.
    pub focused: bool,
    pub hover: Option<Target>,
    pub menu: Option<RowMenu>,
    /// A short message for the status line, and when it was set.
    pub status: Option<(String, Instant)>,
    /// Rows that fit in the list, from the last layout.
    pub page: usize,
    /// Select this entry once the next listing arrives (after going up).
    pub reselect: Option<String>,
    /// Where the panel is docked, set by the window's layout.
    pub rect: Rect,
    last_click: Option<(Instant, usize)>,
}

impl FilePanel {
    pub fn new(pane: PaneId, remote: Remote, path: Option<String>, show_hidden: bool) -> Self {
        Self {
            pane,
            remote,
            path,
            listing: None,
            loading: false,
            error: None,
            request: 0,
            selected: 0,
            scroll: 0,
            filter: String::new(),
            show_hidden,
            follow: true,
            focused: true,
            hover: None,
            menu: None,
            status: None,
            page: 10,
            reselect: None,
            rect: Rect::default(),
            last_click: None,
        }
    }

    /// The rows shown, in order.
    pub fn rows(&self) -> Vec<Row> {
        let Some(listing) = &self.listing else { return Vec::new() };
        let mut rows = Vec::with_capacity(listing.entries.len() + 1);
        if listing.path != "/" && self.filter.is_empty() {
            rows.push(Row::Up);
        }
        let filter = self.filter.to_lowercase();
        rows.extend(
            listing
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| self.show_hidden || !e.is_hidden() || !filter.is_empty())
                .filter(|(_, e)| filter.is_empty() || e.name.to_lowercase().contains(&filter))
                .map(|(i, _)| Row::Entry(i)),
        );
        rows
    }

    pub fn entry(&self, row: Row) -> Option<&Entry> {
        match row {
            Row::Up => None,
            Row::Entry(i) => self.listing.as_ref()?.entries.get(i),
        }
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        let row = *self.rows().get(self.selected)?;
        self.entry(row)
    }

    /// The folder shown, as the server resolved it.
    pub fn dir(&self) -> Option<&str> {
        self.listing.as_ref().map(|l| l.path.as_str())
    }

    pub fn full_path(&self, entry: &Entry) -> Option<String> {
        Some(remote::join(self.dir()?, &entry.name))
    }

    /// Starts opening a folder; the app runs the listing.
    pub fn navigate(&mut self, path: Option<String>) -> Request {
        self.path = path.clone();
        self.filter.clear();
        self.menu = None;
        Request::List(path)
    }

    pub fn refresh(&mut self) -> Request {
        Request::List(self.dir().map(String::from).or_else(|| self.path.clone()))
    }

    /// Takes a finished listing, keeping the selection on the same entry
    /// when the folder didn't change.
    pub fn set_listing(&mut self, listing: Listing) {
        let same_dir = self.dir() == Some(listing.path.as_str());
        let keep = if same_dir { self.selected_entry().map(|e| e.name.clone()) } else { None };
        let reselect = self.reselect.take().or(keep);
        self.path = Some(listing.path.clone());
        self.listing = Some(listing);
        self.loading = false;
        self.error = None;
        if !same_dir {
            self.scroll = 0;
        }
        self.selected = 0;
        if let Some(name) = reselect {
            let rows = self.rows();
            if let Some(i) = rows.iter().position(|&r| self.entry(r).is_some_and(|e| e.name == name)) {
                self.selected = i;
            }
        }
        self.clamp_selection();
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some((message.into(), Instant::now()));
    }

    fn clamp_selection(&mut self) {
        let count = self.rows().len();
        self.selected = self.selected.min(count.saturating_sub(1));
        self.keep_visible();
    }

    fn keep_visible(&mut self) {
        let page = self.page.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + page {
            self.scroll = self.selected + 1 - page;
        }
        let max = self.rows().len().saturating_sub(page);
        self.scroll = self.scroll.min(max);
    }

    pub fn move_selection(&mut self, delta: isize) {
        let count = self.rows().len();
        if count == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, count as isize - 1) as usize;
        self.keep_visible();
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.rows().len().saturating_sub(self.page.max(1));
        self.scroll = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
    }

    /// Opens a row: folders are entered, files downloaded.
    fn open_row(&mut self, position: usize) -> Option<Request> {
        let row = *self.rows().get(position)?;
        match row {
            Row::Up => self.go_up(),
            Row::Entry(_) => {
                let entry = self.entry(row)?.clone();
                if entry.is_dir() {
                    let path = self.full_path(&entry)?;
                    Some(self.navigate(Some(path)))
                } else {
                    Some(Request::Download(vec![entry]))
                }
            }
        }
    }

    fn go_up(&mut self) -> Option<Request> {
        let dir = self.dir()?.to_string();
        if dir == "/" {
            return None;
        }
        // Land on the folder just left.
        self.reselect = dir.rsplit('/').next().map(String::from);
        Some(self.navigate(Some(remote::parent(&dir))))
    }

    /// Handles a key while the panel has the keyboard.
    pub fn key(&mut self, key: &Key, text: Option<&str>, ctrl: bool, shift: bool) -> Option<Request> {
        if self.menu.take().is_some() && *key == Key::Named(NamedKey::Escape) {
            return None;
        }
        let page = self.page.max(1) as isize;
        match key {
            Key::Named(NamedKey::ArrowUp) => self.move_selection(-1),
            Key::Named(NamedKey::ArrowDown) => self.move_selection(1),
            Key::Named(NamedKey::PageUp) => self.move_selection(-page),
            Key::Named(NamedKey::PageDown) => self.move_selection(page),
            Key::Named(NamedKey::Home) => self.move_selection(isize::MIN / 2),
            Key::Named(NamedKey::End) => self.move_selection(isize::MAX / 2),
            Key::Named(NamedKey::Enter) => return self.open_row(self.selected),
            Key::Named(NamedKey::ArrowRight) if self.selected_entry().is_some_and(Entry::is_dir) => {
                return self.open_row(self.selected);
            }
            Key::Named(NamedKey::ArrowLeft) if self.filter.is_empty() => return self.go_up(),
            Key::Named(NamedKey::Backspace) => {
                if self.filter.is_empty() {
                    return self.go_up();
                }
                self.filter.pop();
                self.selected = 0;
                self.clamp_selection();
            }
            Key::Named(NamedKey::Escape) => {
                if self.filter.is_empty() {
                    return Some(Request::Unfocus);
                }
                self.filter.clear();
                self.clamp_selection();
            }
            Key::Named(NamedKey::Delete) => return self.selected_entry().map(|e| Request::Delete(vec![e.clone()])),
            Key::Named(NamedKey::F2) => return self.selected_entry().map(|e| Request::Rename(e.clone())),
            Key::Named(NamedKey::F5) => return Some(self.refresh()),
            Key::Character(c) if ctrl && !shift => match c.to_lowercase().as_str() {
                "r" => return Some(self.refresh()),
                "n" => return Some(Request::NewFolder),
                "u" => return Some(Request::Upload { folders: false }),
                "d" => return self.selected_entry().map(|e| Request::Download(vec![e.clone()])),
                "h" => {
                    self.show_hidden = !self.show_hidden;
                    self.clamp_selection();
                }
                "c" => return self.selected_entry().and_then(|e| self.full_path(e)).map(Request::CopyPath),
                "l" => {
                    self.filter.clear();
                    self.clamp_selection();
                }
                _ => {}
            },
            _ if !ctrl => {
                // Typing narrows the list.
                if let Some(text) = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control)) {
                    self.filter.push_str(text);
                    self.selected = 0;
                    self.scroll = 0;
                    // Skip the ".." row, which filtering hides anyway.
                    self.clamp_selection();
                }
            }
            _ => {}
        }
        None
    }

    /// Handles a mouse press on a target.
    pub fn click(&mut self, target: &Target, right: bool, x: f32, y: f32) -> Option<Request> {
        let menu = self.menu.take();
        match target {
            Target::Menu(item) => {
                let row = *self.rows().get(menu?.row)?;
                let entry = self.entry(row)?.clone();
                return match item {
                    MenuItem::Open => {
                        let path = self.full_path(&entry)?;
                        Some(self.navigate(Some(path)))
                    }
                    MenuItem::Download => Some(Request::Download(vec![entry])),
                    MenuItem::Rename => Some(Request::Rename(entry)),
                    MenuItem::Delete => Some(Request::Delete(vec![entry])),
                    MenuItem::CopyPath => self.full_path(&entry).map(Request::CopyPath),
                };
            }
            Target::Row(position) => {
                self.selected = *position;
                if right {
                    if self.entry(self.rows()[*position]).is_some() {
                        self.menu = Some(RowMenu { row: *position, x, y });
                    }
                    return None;
                }
                let now = Instant::now();
                let double = self
                    .last_click
                    .is_some_and(|(at, p)| p == *position && now.duration_since(at).as_millis() < DOUBLE_CLICK_MS);
                self.last_click = (!double).then_some((now, *position));
                if double {
                    return self.open_row(*position);
                }
            }
            Target::Button(button) if !right => {
                return match button {
                    Button::Up => self.go_up(),
                    Button::Refresh => Some(self.refresh()),
                    Button::NewFolder => Some(Request::NewFolder),
                    Button::UploadFiles => Some(Request::Upload { folders: false }),
                    Button::UploadFolder => Some(Request::Upload { folders: true }),
                    Button::Hidden => {
                        self.show_hidden = !self.show_hidden;
                        self.clamp_selection();
                        None
                    }
                    Button::Follow => {
                        self.follow = !self.follow;
                        self.set_status(if self.follow {
                            "Following the terminal's directory"
                        } else {
                            "Not following the terminal"
                        });
                        self.follow.then_some(Request::Follow)
                    }
                    Button::Close => Some(Request::Close),
                };
            }
            Target::Crumb(path) if !right => return Some(self.navigate(Some(path.clone()))),
            Target::Transfer(id, part) if !right => {
                return Some(match part {
                    TransferPart::Close => Request::Cancel(*id),
                    TransferPart::Reveal => Request::Reveal(*id),
                });
            }
            _ => {}
        }
        None
    }

    fn menu_items(&self, row: usize) -> Vec<MenuItem> {
        let is_dir = self.rows().get(row).and_then(|&r| self.entry(r)).is_some_and(Entry::is_dir);
        let mut items = Vec::with_capacity(5);
        if is_dir {
            items.push(MenuItem::Open);
        }
        items.extend([MenuItem::Download, MenuItem::Rename, MenuItem::Delete, MenuItem::CopyPath]);
        items
    }
}

//#region Layout

pub struct TransferLayout {
    pub id: TransferId,
    pub rect: Rect,
    pub close: Rect,
    pub reveal: Option<Rect>,
}

pub struct PanelLayout {
    pub rect: Rect,
    pub header: Rect,
    pub buttons: Vec<(Button, Rect)>,
    pub crumb_row: Rect,
    /// (folder path, label, rect)
    pub crumbs: Vec<(String, String, Rect)>,
    pub filter: Option<Rect>,
    pub list: Rect,
    /// (row position, rect)
    pub rows: Vec<(usize, Rect)>,
    pub row_height: f32,
    pub status: Rect,
    pub transfers: Vec<TransferLayout>,
    /// Transfers not shown for lack of room.
    pub more_transfers: usize,
    pub menu: Option<(Rect, Vec<(MenuItem, Rect)>)>,
}

/// The panel's width for a tab area.
pub fn width(columns: u16, m: CellMetrics, area: Rect) -> f32 {
    (f32::from(columns) * m.width).min(area.width * 0.5).max(m.width * 24.0).round()
}

pub fn layout(panel: &FilePanel, transfers: &[&Transfer], rect: Rect, m: CellMetrics, scale: f32) -> PanelLayout {
    let px = |v: f32| (v * scale).round();
    let pad = px(8.0);
    let header_h = (m.height * 2.0).round();
    let row_h = (m.height * 1.45).round();
    let x = rect.x;
    let inner_x = x + pad;
    let inner_w = rect.width - 2.0 * pad;

    // Header: title on the left, buttons on the right.
    let header = Rect::new(x, rect.y, rect.width, header_h);
    let button_w = (m.width * 2.6).round();
    let button_h = (m.height * 1.5).round();
    let mut bx = x + rect.width - pad;
    let mut buttons: Vec<(Button, Rect)> = Button::ALL
        .iter()
        .rev()
        .map(|&b| {
            bx -= button_w;
            (b, Rect::new(bx, rect.y + ((header_h - button_h) / 2.0).round(), button_w, button_h))
        })
        .collect();
    buttons.reverse();

    // Breadcrumbs.
    let crumb_row = Rect::new(inner_x, rect.y + header_h, inner_w, row_h);
    let crumbs = crumb_layout(panel.dir().or(panel.path.as_deref()), crumb_row, m);
    let mut y = crumb_row.y + row_h + px(2.0);
    let filter = (!panel.filter.is_empty()).then(|| {
        let r = Rect::new(inner_x, y, inner_w, row_h);
        y += row_h + px(2.0);
        r
    });

    // Bottom up: transfers, then the status line.
    let (transfer_layouts, more_transfers, top) =
        layout_transfers(transfers, inner_x, inner_w, rect.y + rect.height - pad, MAX_TRANSFERS_SHOWN, m, scale);
    let bottom = if transfer_layouts.is_empty() { top } else { top - px(6.0) };
    let status = Rect::new(inner_x, bottom - row_h, inner_w, row_h);
    let list_bottom = status.y - px(2.0);

    let list = Rect::new(x, y, rect.width, (list_bottom - y).max(0.0));
    let capacity = (list.height / row_h).floor().max(1.0) as usize;
    let count = panel.rows().len();
    let rows = (panel.scroll..count.min(panel.scroll + capacity))
        .enumerate()
        .map(|(slot, position)| (position, Rect::new(inner_x, y + slot as f32 * row_h, inner_w, row_h)))
        .collect();

    let menu = panel.menu.as_ref().map(|open| {
        let items = panel.menu_items(open.row);
        let item_h = (m.height * 1.5).round();
        let w = (m.width * 18.0).round();
        let h = items.len() as f32 * item_h + 2.0 * px(4.0);
        let mx = open.x.min(rect.x + rect.width - w - px(4.0)).max(rect.x + px(4.0));
        let my = open.y.min(rect.y + rect.height - h - px(4.0)).max(rect.y);
        let panel_rect = Rect::new(mx, my, w, h);
        let entries = items
            .into_iter()
            .enumerate()
            .map(|(i, item)| (item, Rect::new(mx + px(4.0), my + px(4.0) + i as f32 * item_h, w - px(8.0), item_h)))
            .collect();
        (panel_rect, entries)
    });

    PanelLayout {
        rect,
        header,
        buttons,
        crumb_row,
        crumbs,
        filter,
        list,
        rows,
        row_height: row_h,
        status,
        transfers: transfer_layouts,
        more_transfers,
        menu,
    }
}

/// Stacks transfer rows upward from `bottom`, at most `max` of them.
/// Returns their layouts, how many didn't fit, and the top of the stack.
pub fn layout_transfers(
    transfers: &[&Transfer],
    x: f32,
    width: f32,
    bottom: f32,
    max: usize,
    m: CellMetrics,
    scale: f32,
) -> (Vec<TransferLayout>, usize, f32) {
    let px = |v: f32| (v * scale).round();
    let transfer_h = (m.height * 2.6).round();
    let gap = px(4.0);
    let mut bottom = bottom;
    let shown = transfers.len().min(max);
    let more = transfers.len() - shown;
    if more > 0 {
        bottom -= m.height;
    }
    let mut layouts: Vec<TransferLayout> = transfers[..shown]
        .iter()
        .rev()
        .map(|t| {
            bottom -= transfer_h;
            let r = Rect::new(x, bottom, width, transfer_h);
            bottom -= gap;
            let size = (m.height * 1.3).round();
            let close = Rect::new(r.x + r.width - size, r.y + px(2.0), size, size);
            let reveal = (t.direction == Direction::Download && t.state == TransferState::Done)
                .then(|| Rect::new(close.x - size - px(2.0), close.y, size, size));
            TransferLayout {
                id: t.id,
                rect: r,
                close,
                reveal,
            }
        })
        .collect();
    layouts.reverse();
    (layouts, more, bottom + gap)
}

/// Breadcrumb segments that fit the row, dropping the leftmost ones (shown
/// as `…`) when the path is too long.
fn crumb_layout(path: Option<&str>, row: Rect, m: CellMetrics) -> Vec<(String, String, Rect)> {
    let Some(path) = path else { return Vec::new() };
    let mut segments: Vec<(String, String)> = vec![("/".into(), "/".into())];
    let mut so_far = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        so_far.push('/');
        so_far.push_str(part);
        segments.push((so_far.clone(), part.to_string()));
    }
    let cols = (row.width / m.width) as usize;
    // Each segment after the root is followed by " / " except the last.
    let width = |segs: &[(String, String)]| -> usize {
        segs.iter().map(|(_, l)| text_columns(l)).sum::<usize>() + 3 * segs.len().saturating_sub(1)
    };
    let mut first = 0;
    while first + 1 < segments.len() && width(&segments[first..]) + if first > 0 { 4 } else { 0 } > cols {
        first += 1;
    }
    let mut x = row.x;
    let mut out = Vec::new();
    if first > 0 {
        let (path, _) = &segments[first - 1];
        out.push((path.clone(), "…".to_string(), Rect::new(x, row.y, m.width, row.height)));
        x += 4.0 * m.width;
    }
    for (i, (path, label)) in segments[first..].iter().enumerate() {
        let label = if first == segments.len() - 1 { truncate(label, cols) } else { label.clone() };
        let w = text_columns(&label) as f32 * m.width;
        out.push((path.clone(), label, Rect::new(x, row.y, w, row.height)));
        x += w;
        // The root "/" is its own separator.
        if !(first == 0 && i == 0) {
            x += 3.0 * m.width;
        } else {
            x += m.width;
        }
    }
    out
}

/// What's under a point. `None` = outside the panel.
pub fn hit(layout: &PanelLayout, x: f32, y: f32) -> Option<Target> {
    if let Some((menu, items)) = &layout.menu
        && menu.contains(x, y)
    {
        return Some(
            items
                .iter()
                .find(|(_, r)| r.contains(x, y))
                .map_or(Target::Empty, |(item, _)| Target::Menu(*item)),
        );
    }
    if !layout.rect.contains(x, y) {
        return None;
    }
    if let Some((button, _)) = layout.buttons.iter().find(|(_, r)| r.contains(x, y)) {
        return Some(Target::Button(*button));
    }
    if let Some((path, _, _)) = layout.crumbs.iter().find(|(_, _, r)| r.contains(x, y)) {
        return Some(Target::Crumb(path.clone()));
    }
    for t in &layout.transfers {
        if t.close.contains(x, y) {
            return Some(Target::Transfer(t.id, TransferPart::Close));
        }
        if t.reveal.is_some_and(|r| r.contains(x, y)) {
            return Some(Target::Transfer(t.id, TransferPart::Reveal));
        }
    }
    if let Some((position, _)) = layout.rows.iter().find(|(_, r)| r.contains(x, y)) {
        return Some(Target::Row(*position));
    }
    Some(Target::Empty)
}

//#endregion

//#region Drawing

/// A Nerd Font icon and colour for an entry.
pub fn entry_icon(entry: &Entry, palette: &[Rgb], muted: Rgb) -> (char, Rgb) {
    let blue = palette[4];
    match entry.kind {
        EntryKind::Dir => return ('\u{F07B}', blue),
        EntryKind::Link { to_dir: true } => return ('\u{F482}', palette[6]),
        EntryKind::Link { to_dir: false } => return ('\u{F481}', palette[6]),
        EntryKind::Other => return ('\u{F016}', palette[3]),
        EntryKind::File => {}
    }
    let lower = entry.name.to_lowercase();
    let ext = lower.rsplit_once('.').map_or("", |(_, e)| e);
    match (lower.as_str(), ext) {
        ("dockerfile" | "docker-compose.yml" | "compose.yml" | "compose.yaml", _) => ('\u{F308}', blue),
        ("makefile", _) => ('\u{E779}', muted),
        (_, "rs") => ('\u{E7A8}', palette[3]),
        (_, "py") => ('\u{E73C}', palette[3]),
        (_, "js" | "mjs" | "cjs") => ('\u{E74E}', palette[3]),
        (_, "ts" | "tsx" | "mts") => ('\u{E628}', blue),
        (_, "jsx") => ('\u{E7BA}', palette[6]),
        (_, "json" | "jsonc") => ('\u{E60B}', palette[3]),
        (_, "md" | "markdown") => ('\u{F48A}', muted),
        (_, "toml" | "yaml" | "yml" | "ini" | "conf" | "cfg" | "env" | "properties") => ('\u{E615}', muted),
        (_, "sh" | "bash" | "zsh" | "fish") => ('\u{F489}', palette[2]),
        (_, "html" | "htm") => ('\u{E736}', palette[1]),
        (_, "css" | "scss" | "sass") => ('\u{E749}', blue),
        (_, "go") => ('\u{E627}', palette[6]),
        (_, "c" | "h") => ('\u{E61E}', blue),
        (_, "cpp" | "cc" | "hpp") => ('\u{E61D}', blue),
        (_, "java" | "jar") => ('\u{E738}', palette[1]),
        (_, "php") => ('\u{E73D}', palette[5]),
        (_, "lua") => ('\u{E620}', blue),
        (_, "dart") => ('\u{E798}', blue),
        (_, "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" | "avif") => ('\u{F1C5}', palette[5]),
        (_, "mp4" | "mkv" | "avi" | "webm" | "mov" | "m4v") => ('\u{F1C8}', palette[5]),
        (_, "mp3" | "flac" | "wav" | "ogg" | "m4a" | "opus") => ('\u{F1C7}', palette[5]),
        (_, "zip" | "tar" | "gz" | "tgz" | "xz" | "zst" | "7z" | "rar" | "bz2" | "deb" | "rpm") => ('\u{F1C6}', palette[1]),
        (_, "pdf") => ('\u{F1C1}', palette[1]),
        (_, "txt" | "log") => ('\u{F15C}', muted),
        (_, "iso" | "img" | "qcow2") => ('\u{F0A0}', muted),
        (_, "db" | "sqlite" | "sqlite3" | "sql") => ('\u{F1C0}', muted),
        (_, "pem" | "key" | "pub" | "crt" | "cer") => ('\u{F084}', palette[3]),
        (_, "lock") => ('\u{F023}', muted),
        _ if entry.is_executable() => ('\u{F489}', palette[2]),
        _ => ('\u{F15B}', muted),
    }
}

/// `512 B`, `4.0 K`, `12 M`, `1.2 G`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["K", "M", "G", "T", "P"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// How long ago, compactly: `now`, `5m`, `3h`, `4d`, `2mo`, `3y`.
pub fn age(mtime: i64, now: i64) -> String {
    let secs = (now - mtime).max(0);
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        86_400..2_592_000 => format!("{}d", secs / 86_400),
        2_592_000..31_536_000 => format!("{}mo", secs / 2_592_000),
        _ => format!("{}y", secs / 31_536_000),
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub fn draw(
    panel: &FilePanel,
    layout: &PanelLayout,
    transfers: &[&Transfer],
    theme: &Theme,
    m: CellMetrics,
    scale: f32,
    out: &mut Vec<ChromeItem>,
) {
    let bg = theme.background.0;
    let fg = theme.foreground.0;
    let palette = theme.palette().colors;
    let panel_bg = mix(bg, fg, 0.035);
    let muted = mix(fg, panel_bg, 0.45);
    let faint = mix(fg, panel_bg, 0.65);
    let accent = theme.cursor.0;
    let edge = scale.round().max(1.0);
    let small = (5.0 * scale).round();
    let r = layout.rect;
    let rounded = |rect: Rect, radius: f32, color: Rgb| ChromeItem::RoundedRect {
        rect: [rect.x, rect.y, rect.width, rect.height],
        radius,
        top_only: false,
        color,
        alpha: 1.0,
    };
    let text = |text: String, x: f32, y: f32, color: Rgb| ChromeItem::Text {
        text,
        x,
        y,
        color,
        bold: false,
    };
    let center_y = |rect: Rect| rect.y + ((rect.height - m.height) / 2.0).round();

    out.push(ChromeItem::Rect {
        rect: [r.x, r.y, r.width, r.height],
        color: panel_bg,
        alpha: 1.0,
    });
    // The divider toward the terminal, brighter while the panel has the keyboard.
    out.push(ChromeItem::Rect {
        rect: [r.x, r.y, edge, r.height],
        color: if panel.focused { mix(panel_bg, accent, 0.7) } else { mix(panel_bg, fg, 0.15) },
        alpha: 1.0,
    });

    // Header.
    let h = layout.header;
    let first_button = layout.buttons.first().map_or(h.x + h.width, |(_, b)| b.x);
    let title_x = h.x + (10.0 * scale).round();
    let title_cols = ((first_button - title_x) / m.width) as usize;
    out.push(text("\u{F233}".into(), title_x, center_y(h), accent));
    out.push(ChromeItem::Text {
        text: truncate(&panel.remote.alias, title_cols.saturating_sub(3)),
        x: title_x + 2.0 * m.width,
        y: center_y(h),
        color: fg,
        bold: true,
    });
    for (button, b) in &layout.buttons {
        let hovered = panel.hover == Some(Target::Button(*button));
        let on = match button {
            Button::Follow => panel.follow,
            Button::Hidden => panel.show_hidden,
            _ => false,
        };
        if hovered || on {
            out.push(rounded(*b, small, mix(panel_bg, fg, if hovered { 0.14 } else { 0.07 })));
        }
        let color = match button {
            Button::Follow | Button::Hidden if on => accent,
            Button::Close if hovered => palette[1],
            _ if hovered => fg,
            _ => muted,
        };
        let icon = button.icon(panel).to_string();
        out.push(text(icon, b.x + ((b.width - m.width) / 2.0).round(), center_y(*b), color));
    }
    out.push(ChromeItem::Rect {
        rect: [r.x + edge, h.y + h.height - edge, r.width - edge, edge],
        color: mix(panel_bg, fg, 0.1),
        alpha: 1.0,
    });

    // Breadcrumbs.
    let crumb_y = center_y(layout.crumb_row);
    let crumb_count = layout.crumbs.len();
    for (i, (path, label, cr)) in layout.crumbs.iter().enumerate() {
        let hovered = panel.hover == Some(Target::Crumb(path.clone()));
        let last = i + 1 == crumb_count;
        let color = if hovered {
            accent
        } else if last {
            fg
        } else {
            muted
        };
        out.push(text(label.clone(), cr.x, crumb_y, color));
        if !last && label != "/" {
            out.push(text("/".into(), cr.x + cr.width + m.width, crumb_y, faint));
        }
    }

    // Filter.
    if let Some(fr) = layout.filter {
        out.push(rounded(fr, small, mix(bg, fg, 0.02)));
        out.push(text("\u{F0B0}".into(), fr.x + m.width * 0.5, center_y(fr), accent));
        let cols = ((fr.width / m.width) as usize).saturating_sub(4);
        let chars: Vec<char> = panel.filter.chars().collect();
        let shown: String = chars[chars.len().saturating_sub(cols)..].iter().collect();
        let shown_cols = text_columns(&shown);
        out.push(text(shown, fr.x + m.width * 2.5, center_y(fr), fg));
        if panel.focused {
            out.push(ChromeItem::Rect {
                rect: [fr.x + m.width * (2.5 + shown_cols as f32), center_y(fr), (2.0 * scale).round(), m.height],
                color: accent,
                alpha: 1.0,
            });
        }
    }

    // Rows.
    let rows = panel.rows();
    let now = unix_now();
    let size_cols = 6;
    let age_cols = 4;
    for &(position, rr) in &layout.rows {
        let row = rows[position];
        let selected = position == panel.selected;
        let hovered = panel.hover == Some(Target::Row(position));
        if selected || hovered {
            let strength = match (selected, panel.focused) {
                (true, true) => 0.14,
                (true, false) => 0.08,
                _ => 0.05,
            };
            out.push(rounded(rr, small, mix(panel_bg, fg, strength)));
        }
        if selected && panel.focused {
            out.push(rounded(
                Rect::new(rr.x, rr.y + (rr.height * 0.25).round(), (3.0 * scale).round(), (rr.height * 0.5).round()),
                (1.5 * scale).round(),
                accent,
            ));
        }
        let ty = center_y(rr);
        let ix = rr.x + (8.0 * scale).round();
        let total_cols = ((rr.x + rr.width - ix) / m.width) as usize;
        match row {
            Row::Up => {
                out.push(text("\u{F062}".into(), ix, ty, muted));
                out.push(text("..".into(), ix + 2.0 * m.width, ty, muted));
            }
            Row::Entry(_) => {
                let Some(entry) = panel.entry(row) else { continue };
                let (icon, icon_color) = entry_icon(entry, &palette, muted);
                let name_color = match entry.kind {
                    EntryKind::Dir => palette[4],
                    EntryKind::Link { .. } => palette[6],
                    _ if entry.is_executable() => palette[2],
                    _ => fg,
                };
                let name_color = if entry.is_hidden() { mix(name_color, panel_bg, 0.35) } else { name_color };
                out.push(text(icon.to_string(), ix, ty, icon_color));
                let meta_cols = size_cols + age_cols + 2;
                let name_cols = total_cols.saturating_sub(3 + meta_cols);
                let name = if entry.is_dir() { format!("{}/", entry.name) } else { entry.name.clone() };
                out.push(text(truncate(&name.replace('\n', "␤"), name_cols), ix + 2.0 * m.width, ty, name_color));
                let right = rr.x + rr.width - m.width;
                if let Some(mtime) = entry.mtime {
                    let a = age(mtime, now);
                    out.push(text(a.clone(), right - text_columns(&a) as f32 * m.width, ty, faint));
                }
                if !entry.is_dir() {
                    let s = human_size(entry.size);
                    let sx = right - (age_cols + 1) as f32 * m.width - text_columns(&s) as f32 * m.width;
                    out.push(text(s, sx, ty, muted));
                }
            }
        }
    }
    // Scroll position, when the list is longer than the view.
    if rows.len() > layout.rows.len() && !layout.rows.is_empty() {
        let list = layout.list;
        let track_h = list.height;
        let thumb_h = (track_h * layout.rows.len() as f32 / rows.len() as f32).max(m.height);
        let thumb_y = list.y + (track_h - thumb_h) * panel.scroll as f32 / (rows.len() - layout.rows.len()) as f32;
        out.push(rounded(
            Rect::new(r.x + r.width - (4.0 * scale).round(), thumb_y, (3.0 * scale).round(), thumb_h),
            (1.5 * scale).round(),
            mix(panel_bg, fg, 0.25),
        ));
    }

    // Empty and loading states.
    let message = if let Some(error) = &panel.error {
        Some((error.clone(), palette[1]))
    } else if panel.listing.is_none() {
        Some(("Loading…".to_string(), muted))
    } else if rows.iter().all(|r| *r == Row::Up) {
        Some((if panel.filter.is_empty() { "Empty folder" } else { "Nothing matches" }.to_string(), muted))
    } else {
        None
    };
    if let Some((message, color)) = message {
        let list = layout.list;
        let cols = (list.width / m.width) as usize - 2;
        let y = list.y + layout.row_height * if rows.is_empty() { 0.5 } else { 1.5 };
        for (i, line) in crate::dialog::wrap(&message, cols).into_iter().take(4).enumerate() {
            out.push(text(line, list.x + m.width, y + i as f32 * m.height * 1.2, color));
        }
    }

    // Status line: a hovered button's tooltip, a message, or the selection.
    let s = layout.status;
    out.push(ChromeItem::Rect {
        rect: [r.x + edge, s.y - edge, r.width - edge, edge],
        color: mix(panel_bg, fg, 0.08),
        alpha: 1.0,
    });
    let status_cols = (s.width / m.width) as usize;
    let fresh_status = panel.status.as_ref().filter(|(_, at)| at.elapsed().as_secs() < 4).map(|(t, _)| t.clone());
    let (status, color) = match (&panel.hover, fresh_status) {
        (Some(Target::Button(b)), _) => (b.tooltip().to_string(), muted),
        (_, Some(message)) => (message, fg),
        _ if panel.loading && panel.listing.is_some() => ("Loading…".to_string(), muted),
        _ => match panel.selected_entry() {
            Some(e) => {
                let mut info = format!("{} {}", e.mode, e.owner);
                if !e.is_dir() {
                    info.push_str(&format!("  {}", human_size(e.size)));
                }
                if let Some(target) = &e.target {
                    info.push_str(&format!("  → {target}"));
                }
                (info, faint)
            }
            None => (
                panel.listing.as_ref().map_or_else(String::new, |l| {
                    let count = l.entries.len();
                    let hidden = if panel.show_hidden { 0 } else { l.entries.iter().filter(|e| e.is_hidden()).count() };
                    let mut status = format!("{count} item{}", if count == 1 { "" } else { "s" });
                    if hidden > 0 {
                        status.push_str(&format!(" ({hidden} hidden)"));
                    }
                    status
                }),
                faint,
            ),
        },
    };
    out.push(text(truncate(&status, status_cols), s.x, center_y(s), color));

    // Transfers.
    draw_transfers(transfers, &layout.transfers, panel.hover.as_ref(), panel_bg, theme, m, scale, out);
    if layout.more_transfers > 0 {
        let last = layout.transfers.last().map_or(s.y + s.height, |t| t.rect.y + t.rect.height);
        out.push(text(format!("+{} more", layout.more_transfers), s.x, last, muted));
    }

    // Row menu, on top of everything.
    if let Some((mr, items)) = &layout.menu {
        out.push(rounded(
            Rect::new(mr.x - edge, mr.y - edge, mr.width + 2.0 * edge, mr.height + 2.0 * edge),
            (7.0 * scale).round() + edge,
            mix(bg, fg, 0.22),
        ));
        let menu_bg = mix(bg, fg, 0.09);
        out.push(rounded(*mr, (7.0 * scale).round(), menu_bg));
        for (item, ir) in items {
            if panel.hover == Some(Target::Menu(*item)) {
                out.push(rounded(*ir, small, mix(menu_bg, fg, 0.12)));
            }
            let color = if *item == MenuItem::Delete { palette[1] } else { fg };
            out.push(text(item.label().into(), ir.x + m.width, center_y(*ir), color));
        }
    }
}

/// Draws transfer rows: name, progress bar, speed, and their buttons.
#[allow(clippy::too_many_arguments)]
pub fn draw_transfers(
    transfers: &[&Transfer],
    layouts: &[TransferLayout],
    hover: Option<&Target>,
    panel_bg: Rgb,
    theme: &Theme,
    m: CellMetrics,
    scale: f32,
    out: &mut Vec<ChromeItem>,
) {
    let fg = theme.foreground.0;
    let palette = theme.palette().colors;
    let muted = mix(fg, panel_bg, 0.45);
    let accent = theme.cursor.0;
    let small = (5.0 * scale).round();
    let rounded = |rect: Rect, radius: f32, color: Rgb| ChromeItem::RoundedRect {
        rect: [rect.x, rect.y, rect.width, rect.height],
        radius,
        top_only: false,
        color,
        alpha: 1.0,
    };
    let text = |text: String, x: f32, y: f32, color: Rgb| ChromeItem::Text {
        text,
        x,
        y,
        color,
        bold: false,
    };
    let center_y = |rect: Rect| rect.y + ((rect.height - m.height) / 2.0).round();
    for tl in layouts {
        let Some(t) = transfers.iter().find(|t| t.id == tl.id) else { continue };
        let tr = tl.rect;
        out.push(rounded(tr, small, mix(panel_bg, fg, 0.05)));
        let line1 = tr.y + (4.0 * scale).round();
        let arrow = match t.direction {
            Direction::Download => '\u{F019}',
            Direction::Upload => '\u{F093}',
        };
        let state_color = match &t.state {
            TransferState::Done => palette[2],
            TransferState::Failed(_) => palette[1],
            TransferState::Cancelled => muted,
            _ => accent,
        };
        out.push(text(arrow.to_string(), tr.x + m.width * 0.5, line1, state_color));
        let name_right = tl.reveal.map_or(tl.close.x, |r| r.x);
        let name_cols = (((name_right - tr.x) / m.width) as usize).saturating_sub(3);
        let name = if t.is_dir { format!("{}/", t.name) } else { t.name.clone() };
        out.push(text(truncate(&name, name_cols), tr.x + m.width * 2.5, line1, fg));
        let close_hover = hover == Some(&Target::Transfer(t.id, TransferPart::Close));
        out.push(text(
            "\u{F00D}".into(),
            tl.close.x + ((tl.close.width - m.width) / 2.0).round(),
            center_y(tl.close),
            if close_hover { palette[1] } else { muted },
        ));
        if let Some(rv) = tl.reveal {
            let hovered = hover == Some(&Target::Transfer(t.id, TransferPart::Reveal));
            out.push(text(
                "\u{F07C}".into(),
                rv.x + ((rv.width - m.width) / 2.0).round(),
                center_y(rv),
                if hovered { accent } else { muted },
            ));
        }
        // Progress bar and details.
        let bar_y = tr.y + tr.height - (m.height * 1.05).round();
        let detail = transfer_detail(t);
        let detail_cols = text_columns(&detail);
        let bar_x = tr.x + m.width * 0.5;
        let bar_w = (tr.width - m.width * (detail_cols as f32 + 2.0)).max(m.width * 4.0);
        let bar_h = (3.0 * scale).round();
        let bar_top = bar_y + ((m.height - bar_h) / 2.0).round();
        out.push(rounded(Rect::new(bar_x, bar_top, bar_w, bar_h), bar_h / 2.0, mix(panel_bg, fg, 0.12)));
        let fraction = t.fraction().unwrap_or(0.0);
        if fraction > 0.0 {
            out.push(rounded(
                Rect::new(bar_x, bar_top, (bar_w * fraction).max(bar_h), bar_h),
                bar_h / 2.0,
                state_color,
            ));
        }
        let detail_color = if matches!(t.state, TransferState::Failed(_)) { palette[1] } else { muted };
        out.push(text(detail, bar_x + bar_w + m.width, bar_y, detail_color));
    }
}

/// The text after a transfer's progress bar.
fn transfer_detail(t: &Transfer) -> String {
    match &t.state {
        TransferState::Queued => "waiting".into(),
        TransferState::Done => {
            let done = match t.direction {
                Direction::Download => format!("{} saved", human_size(t.bytes)),
                Direction::Upload => format!("{} sent", human_size(t.bytes)),
            };
            match t.method {
                Some(method) => format!("{done} via {method}"),
                None => done,
            }
        }
        TransferState::Cancelled => "cancelled".into(),
        TransferState::Failed(message) => truncate(message, 28),
        TransferState::Running => {
            let speed = if t.speed > 0.0 { format!("  {}/s", human_size(t.speed as u64)) } else { String::new() };
            match t.fraction() {
                Some(f) => format!("{:>3}%{speed}", (f * 100.0) as u32),
                None => format!("{}{speed}", human_size(t.bytes)),
            }
        }
    }
}

//#endregion

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            name: name.into(),
            kind,
            size: 10,
            mtime: None,
            mode: "-rw-r--r--".into(),
            owner: "ion".into(),
            target: None,
        }
    }

    fn panel() -> FilePanel {
        let mut p = FilePanel::new(1, Remote::with_argv(vec!["true".into()]), None, false);
        p.set_listing(Listing {
            path: "/home/ion".into(),
            entries: vec![
                entry("apps", EntryKind::Dir),
                entry(".config", EntryKind::Dir),
                entry("notes.txt", EntryKind::File),
                entry("photo.png", EntryKind::File),
            ],
        });
        p
    }

    #[test]
    fn rows_hide_dotfiles_and_filter() {
        let mut p = panel();
        assert_eq!(p.rows(), [Row::Up, Row::Entry(0), Row::Entry(2), Row::Entry(3)]);
        p.show_hidden = true;
        assert_eq!(p.rows().len(), 5);
        p.key(&Key::Character("n".into()), Some("n"), false, false);
        p.key(&Key::Character("o".into()), Some("o"), false, false);
        assert_eq!(p.rows(), [Row::Entry(2)]);
        assert_eq!(p.selected_entry().unwrap().name, "notes.txt");
    }

    #[test]
    fn navigation_requests() {
        let mut p = panel();
        // Row 1 is "apps": Enter opens it.
        p.move_selection(1);
        assert_eq!(p.key(&Key::Named(NamedKey::Enter), None, false, false), Some(Request::List(Some("/home/ion/apps".into()))));
        let mut p = panel();
        assert_eq!(p.key(&Key::Named(NamedKey::Backspace), None, false, false), Some(Request::List(Some("/home".into()))));
        assert_eq!(p.reselect.as_deref(), Some("ion"));
        let mut p = panel();
        p.move_selection(2);
        assert!(matches!(p.key(&Key::Named(NamedKey::Enter), None, false, false), Some(Request::Download(e)) if e[0].name == "notes.txt"));
    }

    #[test]
    fn formats_sizes_and_ages() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(4096), "4.0 K");
        assert_eq!(human_size(12 * 1024 * 1024), "12 M");
        assert_eq!(age(100, 100), "now");
        assert_eq!(age(0, 7200), "2h");
        assert_eq!(age(0, 86_400 * 3), "3d");
    }

    #[test]
    fn crumbs_fit() {
        let m = CellMetrics {
            width: 10.0,
            height: 20.0,
            baseline: 15.0,
            underline_offset: 2.0,
            underline_thickness: 1.0,
            strikeout_offset: 5.0,
        };
        let full = crumb_layout(Some("/home/ion"), Rect::new(0.0, 0.0, 400.0, 20.0), m);
        let labels: Vec<&str> = full.iter().map(|(_, l, _)| l.as_str()).collect();
        assert_eq!(labels, ["/", "home", "ion"]);
        let tight = crumb_layout(Some("/very/long/path/to/somewhere"), Rect::new(0.0, 0.0, 150.0, 20.0), m);
        assert_eq!(tight[0].1, "…");
        assert_eq!(tight.last().unwrap().0, "/very/long/path/to/somewhere");
        for (_, _, r) in &tight {
            assert!(r.x + r.width <= 150.0);
        }
    }
}
