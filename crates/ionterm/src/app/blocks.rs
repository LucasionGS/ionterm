//! Command blocks in the window (`crate::blocks`): the sticky header, the
//! scrollbar with its markers, the block handle and menu, folding, and the
//! block actions (copy, run again, open, save).
//!
//! Regular terminal use doesn't change: the handle and scrollbar live in
//! the padding and only show on hover (or right after scrolling), nothing
//! opens by itself, and mouse-reporting apps and the alternate screen get
//! none of it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ionterm_render::ChromeItem;
use ionterm_vt::{FoldChange, TermMode};

use super::{App, TermWindow};
use crate::blocks::{self, Block, BlockAction, BlockMenu, Marker};
use crate::config::Config;
use crate::layout::{Direction, PaneId, Rect};
use crate::theme::Theme;

/// How long the scrollbar stays after the view scrolled.
const SCROLLBAR_LINGER: Duration = Duration::from_millis(1200);

/// Whether a point is in a pane's right padding, where its scrollbar is:
/// right of the text, so clicks on the text still select.
fn in_scrollbar_strip(pane: &crate::pane::Pane, m: ionterm_render::CellMetrics, x: f32, y: f32) -> bool {
    let grid_right = pane.origin[0] + pane.grid.0 as f32 * m.width;
    pane.area.contains(x, y) && x >= grid_right && pane.area.x + pane.area.width - grid_right >= 3.0
}

impl TermWindow {
    /// The sticky header a visible pane shows now, if any.
    pub(super) fn sticky_for(&self, pane: PaneId, config: &Config) -> Option<blocks::Sticky> {
        if !config.terminal.sticky_command || self.settings_page().is_some() {
            return None;
        }
        let pane = self.tabs[self.active_tab].pane(pane)?;
        let running = pane.command.as_ref().map(|(started, _)| started.elapsed());
        blocks::sticky(&pane.lock(), running)
    }

    /// Where a pane's sticky header is drawn (over its top row).
    fn sticky_rect(&self, pane: PaneId) -> Option<Rect> {
        let pane = self.tabs[self.active_tab].pane(pane)?;
        let m = self.renderer.cell_metrics();
        Some(Rect::new(pane.area.x, pane.area.y, pane.area.width, pane.origin[1] + m.height - pane.area.y))
    }

    /// Whether a pane's scrollbar shows: there's history, it isn't a
    /// full-screen app, and it was just scrolled or the mouse is near.
    fn scrollbar_shown(&self, pane: PaneId, config: &Config, now: Instant) -> bool {
        if !config.terminal.scrollbar || self.settings_page().is_some() {
            return false;
        }
        let Some(p) = self.tabs[self.active_tab].pane(pane) else { return false };
        let near = self.scrollbar_hover == Some(pane) || self.scrollbar_drag == Some(pane);
        let recent = p.scrolled_at.is_some_and(|at| now.duration_since(at) < SCROLLBAR_LINGER);
        if !near && !recent {
            return false;
        }
        let term = p.lock();
        term.history_len() > 0 && !term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Hides scrollbars shown after scrolling once they've lingered; returns
    /// when the next one goes.
    pub(super) fn expire_scrollbars(&mut self, now: Instant) -> Option<Instant> {
        let mut next = None;
        let mut expired = false;
        let tab = self.active_tab;
        for pane in &mut self.tabs[tab].panes {
            let Some(at) = pane.scrolled_at else { continue };
            let until = at + SCROLLBAR_LINGER;
            if until <= now {
                pane.scrolled_at = None;
                expired = true;
            } else {
                next = [next, Some(until)].into_iter().flatten().min();
            }
        }
        if expired {
            self.window.request_redraw();
        }
        next
    }

    /// The block under the mouse, for the handle: not while selecting or
    /// dragging, nor where a program takes the mouse.
    pub(super) fn update_block_hover(&mut self) {
        let hover = self.pointer_cell.and_then(|(id, (_, row))| {
            if self.capture.is_some() || self.menu.is_some() || self.palette.is_some() || self.dialog.is_some() {
                return None;
            }
            let pane = self.tabs[self.active_tab].pane(id)?;
            if pane.selecting {
                return None;
            }
            let term = pane.lock();
            if term.mode().intersects(TermMode::MOUSE_REPORTING | TermMode::ALT_SCREEN) {
                return None;
            }
            let block = blocks::block_at(&term, term.viewport_to_absolute(row))?;
            // Only blocks with something to act on (not a bare prompt).
            (block.output.is_some() || block.command.is_some() && block.finished).then_some((id, block.prompt))
        });
        if hover != self.block_hover {
            self.block_hover = hover;
            self.window.request_redraw();
        }
    }

    /// The handle of the hovered block: (pane, the block, its first and last
    /// visible row, where the dots are).
    fn block_handle(&self) -> Option<(PaneId, Block, (isize, isize), Rect)> {
        let (id, prompt) = self.block_hover?;
        let pane = self.tabs[self.active_tab].pane(id)?;
        let term = pane.lock();
        let block = blocks::block_at(&term, prompt)?;
        let rows = term.rows() as isize;
        let first = term.absolute_to_viewport(block.prompt).max(0);
        let last = term.absolute_to_viewport(block.end).min(rows - 1);
        if first > last {
            return None;
        }
        let m = self.renderer.cell_metrics();
        let dots = Rect::new(pane.area.x, pane.origin[1] + first as f32 * m.height, (pane.origin[0] - pane.area.x).max(1.0), m.height);
        Some((id, block, (first, last), dots))
    }

    /// Draws the headers, scrollbars and the block handle of the active tab.
    pub(super) fn draw_blocks(&self, config: &Config, theme: &Theme, out: &mut Vec<ChromeItem>) {
        let tab = &self.tabs[self.active_tab];
        let m = self.renderer.cell_metrics();
        let scale = self.scale_factor as f32;
        let now = Instant::now();
        if let Some((id, _, rows, _)) = self.block_handle()
            && let Some(pane) = tab.pane(id)
        {
            let hovered = self.block_handle_hovered;
            blocks::draw_handle(pane.area, pane.origin, rows, hovered, theme.cursor.0, m, scale, out);
        }
        for pane in tab.panes.iter().filter(|p| tab.is_visible(p.id)) {
            if let Some(sticky) = self.sticky_for(pane.id, config) {
                let hovered = self.sticky_hover == Some(pane.id);
                blocks::draw_sticky(&sticky, pane.area, pane.origin, pane.grid.0, hovered, theme, m, scale, out);
            }
            if self.scrollbar_shown(pane.id, config, now) {
                let track = blocks::scrollbar_track(pane.area, scale);
                let (markers, total, thumb) = {
                    let term = pane.lock();
                    let (total, top) = term.scroll_extent();
                    let mut markers: Vec<(usize, Marker)> = term
                        .prompt_lines()
                        .into_iter()
                        .map(|p| {
                            let failed = term.line(p).and_then(|r| r.status).is_some_and(|s| s != 0);
                            (term.shown_position(p), if failed { Marker::Failed } else { Marker::Prompt })
                        })
                        .collect();
                    if let Some(search) = self.search.as_ref().filter(|s| s.pane == pane.id) {
                        markers.extend(search.bar.matches.iter().map(|&(start, _)| (term.shown_position(start.0), Marker::Match)));
                    }
                    (markers, total, blocks::scrollbar_thumb(track, total, top, term.rows(), scale))
                };
                let hovered = self.scrollbar_hover == Some(pane.id) || self.scrollbar_drag == Some(pane.id);
                blocks::draw_scrollbar(track, thumb, &markers, total, hovered, theme, scale, out);
            }
        }
        if let Some(menu) = &self.block_menu {
            let layout = blocks::menu_layout(menu, m, scale, self.window_rect());
            blocks::draw_menu(menu, &layout, theme, m, scale, out);
        }
    }

    /// Follows the mouse for headers, scrollbars and the handle. Returns
    /// whether the motion was theirs (a scrollbar drag, or over the menu).
    pub(super) fn blocks_motion(&mut self, x: f32, y: f32, config: &Config) -> bool {
        let m = self.renderer.cell_metrics();
        let scale = self.scale_factor as f32;
        if let Some(menu) = &self.block_menu {
            let layout = blocks::menu_layout(menu, m, scale, self.window_rect());
            let hover = blocks::menu_hit(&layout, x, y).flatten();
            if let Some(menu) = &mut self.block_menu
                && menu.hover != hover
            {
                menu.hover = hover;
                self.window.request_redraw();
            }
            self.set_cursor_icon(if hover.is_some() { winit::window::CursorIcon::Pointer } else { winit::window::CursorIcon::Default });
            return true;
        }
        if let Some(id) = self.scrollbar_drag {
            self.scrollbar_jump(id, y);
            return true;
        }
        // A selection being dragged keeps going wherever the mouse is.
        if self.capture.is_some() {
            return false;
        }
        let tab = &self.tabs[self.active_tab];
        let under = tab.panes.iter().find(|p| tab.is_visible(p.id) && p.area.contains(x, y));
        let scrollbar = under.filter(|p| in_scrollbar_strip(p, m, x, y)).map(|p| p.id);
        let under = under.map(|p| (p.id, p.area));
        let sticky = under.map(|(id, _)| id).filter(|&id| {
            self.capture.is_none() && self.sticky_for(id, config).is_some() && self.sticky_rect(id).is_some_and(|r| r.contains(x, y))
        });
        let handle = self.block_handle().is_some_and(|(_, _, _, dots)| dots.contains(x, y));
        if (scrollbar, sticky, handle) != (self.scrollbar_hover, self.sticky_hover, self.block_handle_hovered) {
            self.scrollbar_hover = scrollbar;
            self.sticky_hover = sticky;
            self.block_handle_hovered = handle;
            self.window.request_redraw();
        }
        if handle || sticky.is_some() || scrollbar.is_some_and(|id| self.scrollbar_shown(id, config, Instant::now())) {
            self.set_cursor_icon(winit::window::CursorIcon::Pointer);
            return true;
        }
        false
    }

    /// Scrolls a pane so the scrollbar's line under `y` is in the middle.
    fn scrollbar_jump(&mut self, id: PaneId, y: f32) {
        let scale = self.scale_factor as f32;
        let Some(pane) = self.tabs[self.active_tab].pane_mut(id) else { return };
        let track = blocks::scrollbar_track(pane.area, scale);
        {
            let mut term = pane.lock();
            let (total, _) = term.scroll_extent();
            let rows = term.rows();
            term.scroll_to_shown(blocks::scrollbar_line(track, y, total, rows));
        }
        pane.scrolled_at = Some(Instant::now());
        self.window.request_redraw();
    }
}

impl App {
    /// A click for the block menu, a header, a scrollbar, the handle or a
    /// folded output's summary. Returns whether it was theirs.
    pub(super) fn blocks_click(&mut self, button: crate::input::MouseButton, pressed: bool) -> bool {
        use crate::input::MouseButton;
        let now = Instant::now();
        let config = &self.config;
        let Some(w) = &mut self.window else { return false };
        let (x, y) = (w.pointer.x as f32, w.pointer.y as f32);
        let m = w.renderer.cell_metrics();
        let scale = w.scale_factor as f32;

        // The menu takes the next click, wherever it lands.
        if let Some(menu) = w.block_menu.take() {
            if !pressed {
                w.block_menu = Some(menu);
                return true;
            }
            w.window.request_redraw();
            let layout = blocks::menu_layout(&menu, m, scale, w.window_rect());
            if let Some(Some(index)) = blocks::menu_hit(&layout, x, y)
                && button == MouseButton::Left
            {
                let action = menu.items[index];
                self.block_action(menu.pane, menu.block, action);
            }
            return true;
        }
        if button != MouseButton::Left {
            return false;
        }
        if !pressed {
            return w.scrollbar_drag.take().is_some();
        }
        let tab = &w.tabs[w.active_tab];
        let Some(under) = tab.panes.iter().find(|p| tab.is_visible(p.id) && p.area.contains(x, y)) else {
            return false;
        };
        let (id, on_strip) = (under.id, in_scrollbar_strip(under, m, x, y));
        // The scrollbar: jump there, and follow a drag.
        if on_strip && w.scrollbar_shown(id, config, now) {
            w.scrollbar_drag = Some(id);
            w.scrollbar_jump(id, y);
            return true;
        }
        // The header: back to the command's prompt.
        if let Some(sticky) = w.sticky_for(id, config)
            && w.sticky_rect(id).is_some_and(|r| r.contains(x, y))
        {
            if let Some(pane) = w.tabs[w.active_tab].pane_mut(id) {
                pane.lock().scroll_to_line(sticky.prompt);
                pane.scrolled_at = Some(now);
            }
            w.window.request_redraw();
            return true;
        }
        // The handle: the block's menu.
        if let Some((pane, block, _, dots)) = w.block_handle()
            && dots.contains(x, y)
        {
            let folded = w.tabs[w.active_tab].pane(pane).is_some_and(|p| p.lock().is_folded(block.prompt));
            w.block_menu = BlockMenu::new(pane, block, folded, x, y);
            w.window.request_redraw();
            return true;
        }
        // A folded output's summary line unfolds it.
        let Some(pane) = w.tabs[w.active_tab].pane(id) else { return false };
        let (_, row) = pane.cell_at(f64::from(x), f64::from(y), m);
        let mut term = pane.lock();
        if term.mode().intersects(TermMode::MOUSE_REPORTING) {
            return false;
        }
        if let Some((prompt, _)) = term.fold_at_row(row) {
            term.toggle_fold(prompt);
            drop(term);
            w.window.request_redraw();
            return true;
        }
        false
    }

    /// A key while the block menu is open. Returns whether it took it.
    pub(super) fn block_menu_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        use winit::keyboard::{Key, NamedKey};
        let Some(w) = &mut self.window else { return false };
        let Some(menu) = &mut w.block_menu else { return false };
        if event.state != winit::event::ElementState::Pressed {
            return true;
        }
        w.window.request_redraw();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => w.block_menu = None,
            Key::Named(NamedKey::ArrowDown) | Key::Named(NamedKey::Tab) => menu.step(1),
            Key::Named(NamedKey::ArrowUp) => menu.step(-1),
            Key::Named(NamedKey::Enter) => {
                if let Some(menu) = w.block_menu.take()
                    && let Some(&action) = menu.hover.and_then(|i| menu.items.get(i))
                {
                    self.block_action(menu.pane, menu.block, action);
                }
            }
            _ => {}
        }
        true
    }

    /// The block keyboard actions work on, in the active pane.
    fn current_block(&self) -> Option<(PaneId, Block)> {
        let w = self.window.as_ref()?;
        let pane = w.active_pane();
        let block = blocks::current_block(&pane.lock(), pane.command.is_some())?;
        Some((pane.id, block))
    }

    /// A block action from the keyboard or the palette, on the current block.
    pub(super) fn current_block_action(&mut self, action: Option<BlockAction>) {
        let Some((pane, block)) = self.current_block() else {
            if let Some(w) = &mut self.window {
                w.show_toast("No command here: this shell isn't set up for shell integration, or nothing ran yet");
            }
            return;
        };
        match action {
            Some(action) => self.block_action(pane, block, action),
            // The menu, by the block's prompt (or the top of the pane).
            None => {
                let Some(w) = &mut self.window else { return };
                let Some(p) = w.tabs[w.active_tab].pane(pane) else { return };
                let m = w.renderer.cell_metrics();
                let folded = p.lock().is_folded(block.prompt);
                let row = p.lock().absolute_to_viewport(block.prompt).max(0);
                let (x, y) = (p.origin[0], p.origin[1] + (row + 1) as f32 * m.height);
                w.block_menu = BlockMenu::new(pane, block, folded, x, y);
                if let Some(menu) = &mut w.block_menu {
                    menu.hover = Some(0);
                }
                w.window.request_redraw();
            }
        }
    }

    /// Does one thing with a block.
    pub(super) fn block_action(&mut self, pane: PaneId, block: Block, action: BlockAction) {
        let Some(w) = &mut self.window else { return };
        let Some(p) = w.tabs.iter().find_map(|t| t.pane(pane)) else { return };
        let output = block.output.map(|(start, end)| p.lock().text_between((start, 0), (end, p.grid.0.saturating_sub(1))));
        let lines = block.output.map_or(0, |(start, end)| end - start + 1);
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        match action {
            BlockAction::CopyCommand => {
                let Some(command) = block.command else { return };
                w.show_toast(format!("Copied {}", crate::tabbar::truncate(&command, 60)));
                self.set_clipboard(command, false);
            }
            BlockAction::CopyOutput => {
                let Some(output) = output else { return };
                w.show_toast(format!("Copied the output ({lines} line{})", plural(lines)));
                self.set_clipboard(output, false);
            }
            BlockAction::CopyMarkdown => {
                let text = blocks::markdown(block.command.as_deref(), output.as_deref().unwrap_or_default());
                w.show_toast("Copied the command and its output as Markdown");
                self.set_clipboard(text, false);
            }
            BlockAction::Rerun => {
                let Some(command) = block.command else { return };
                self.rerun(pane, command);
            }
            BlockAction::OpenInEditor | BlockAction::OpenInPager => {
                let Some(output) = output else { return };
                let cwd = p.working_dir();
                // A file of its own, removed with the others when ionterm quits.
                let key = OUTPUT_FILES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let path = match crate::scrollback::write_file(&output, key) {
                    Ok(path) => path,
                    Err(error) => return w.show_toast(format!("Couldn't write the output out: {error}")),
                };
                let title = block.command.clone().unwrap_or_else(|| "Output".into());
                if action == BlockAction::OpenInPager {
                    let argv = crate::scrollback::pager_command(&self.config.terminal.scrollback_pager, &path, 1);
                    self.open_command_tab(argv, cwd);
                    if let Some(w) = &mut self.window {
                        let tab = w.active_tab;
                        w.tabs[tab].custom_title = Some(crate::tabbar::truncate(&title, 40));
                    }
                } else {
                    let template = self.config.hints.open_file.clone();
                    self.open_file(path, None, None, cwd, &template);
                }
            }
            BlockAction::SaveOutput => {
                let Some(output) = output else { return };
                let dir = crate::files::transfer::download_dir(&self.config.files.download_dir);
                let path = output_file_name(&dir, block.command.as_deref());
                match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, output)) {
                    Ok(()) => w.show_toast(format!("Saved to {}", path.display())),
                    Err(error) => w.show_toast(format!("Couldn't save the output: {error}")),
                }
            }
            BlockAction::Fold => {
                let change = p.lock().toggle_fold(block.prompt);
                match change {
                    // Only the part in the scrollback folds; the summary line says how much.
                    FoldChange::Folded(_) => w.show_toast("Folded the output: click its ⋯ line to unfold"),
                    FoldChange::Unfolded => {}
                    FoldChange::Unfoldable => w.show_toast("Only finished commands with more than a few lines of output fold"),
                }
                w.window.request_redraw();
            }
        }
    }

    /// Runs a command again: typed at the prompt when the shell waits there,
    /// else in a new split beside it (same directory, or same host).
    fn rerun(&mut self, pane: PaneId, command: String) {
        let Some(w) = &mut self.window else { return };
        let Some(p) = w.tabs.iter().find_map(|t| t.pane(pane)) else { return };
        let busy = p.command.is_some() || p.lock().mode().contains(TermMode::ALT_SCREEN);
        if !busy {
            // ctrl+e ctrl+u clears whatever is half typed first.
            let mut bytes = b"\x05\x15".to_vec();
            bytes.extend_from_slice(command.as_bytes());
            bytes.push(b'\r');
            p.write(&bytes);
            p.lock().scroll_to_bottom();
            w.window.request_redraw();
            return;
        }
        let (connection, cwd) = Self::follow_pane(p);
        let metrics = w.renderer.cell_metrics();
        self.focus_pane_in_window(pane);
        let new = self.spawn_pane(metrics, connection, cwd);
        let new_id = new.as_ref().ok().map(|p| p.id);
        self.add_split(Some(Direction::Horizontal), new);
        // Typed ahead: the shell reads it once it's up.
        if let Some(id) = new_id
            && let Some(w) = &self.window
            && let Some(p) = w.tabs.iter().find_map(|t| t.pane(id))
        {
            p.write(format!("{command}\r").as_bytes());
        }
    }

    /// Makes a pane of the window being worked on the active one.
    fn focus_pane_in_window(&mut self, id: PaneId) {
        let Some(w) = &mut self.window else { return };
        let Some(index) = w.tabs.iter().position(|t| t.pane(id).is_some()) else { return };
        w.select_tab(index);
        w.set_active_pane(id);
    }
}

/// Numbers the output files opened in an editor or pager.
static OUTPUT_FILES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 32);

/// A free file name for saved output: the command's first word and the time.
fn output_file_name(dir: &std::path::Path, command: Option<&str>) -> PathBuf {
    let word: String = command
        .and_then(|c| c.split_whitespace().next())
        .map(|w| w.rsplit('/').next().unwrap_or(w).chars().filter(|c| c.is_alphanumeric() || "-_.".contains(*c)).collect())
        .filter(|w: &String| !w.is_empty())
        .unwrap_or_else(|| "output".into());
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut path = dir.join(format!("{word}-{stamp}.txt"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{word}-{stamp} ({n}).txt"));
        n += 1;
    }
    path
}
