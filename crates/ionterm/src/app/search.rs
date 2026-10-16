//! Scrollback search in the window: opening the bar over the active pane,
//! searching again as output arrives, scrolling to matches, and drawing.

use std::time::{Duration, Instant};

use ionterm_render::ChromeItem;
use winit::event::KeyEvent;
use winit::window::CursorIcon;

use super::{App, TermWindow};
use crate::layout::PaneId;
use crate::search::{self, Request, Search};
use crate::theme::Theme;

/// New output re-runs the search at most this often.
const RESEARCH_INTERVAL: Duration = Duration::from_millis(250);

/// The open search: which pane, the bar's state, and whether new output
/// came since the last run.
pub struct SearchState {
    pub pane: PaneId,
    pub bar: Search,
    stale: bool,
    last_run: Instant,
}

impl TermWindow {
    fn search_layout(&self) -> Option<search::SearchLayout> {
        let state = self.search.as_ref()?;
        let pane = self.tabs[self.active_tab].pane(state.pane)?;
        Some(search::layout(pane.area, self.renderer.cell_metrics(), self.scale_factor as f32))
    }

    /// Whether the search bar has the keyboard.
    pub(super) fn search_focused(&self) -> bool {
        self.search.as_ref().is_some_and(|s| s.bar.focused) && self.search_layout().is_some()
    }

    /// Runs the search again over the pane's whole scrollback.
    fn run_search(&mut self) {
        let Some(state) = &mut self.search else { return };
        let Some(pane) = self.tabs.iter().flat_map(|t| &t.panes).find(|p| p.id == state.pane) else { return };
        state.stale = false;
        state.last_run = Instant::now();
        if state.bar.query.is_empty() {
            state.bar.error = None;
            state.bar.set_matches(Vec::new(), 0);
            return;
        }
        match search::compile(state.bar.query.text(), state.bar.regex) {
            Ok(pattern) => {
                let term = pane.lock();
                let matches = search::find_all(&term, &pattern);
                let bottom = term.viewport_to_absolute(term.rows().saturating_sub(1));
                drop(term);
                state.bar.error = None;
                state.bar.set_matches(matches, bottom);
            }
            Err(error) => {
                state.bar.error = Some(error);
                state.bar.set_matches(Vec::new(), 0);
            }
        }
    }

    /// Scrolls so the current match is in view, near the middle.
    fn show_match(&mut self) {
        let Some(state) = &self.search else { return };
        let Some(&(start, _)) = state.bar.current.and_then(|i| state.bar.matches.get(i)) else { return };
        let Some(pane) = self.tabs.iter().flat_map(|t| &t.panes).find(|p| p.id == state.pane) else { return };
        let mut term = pane.lock();
        let top = term.viewport_to_absolute(0);
        let rows = term.rows();
        if start.0 < top || start.0 >= top + rows {
            term.scroll_to_line(start.0.saturating_sub(rows / 2));
        }
    }

    /// New output in a pane: its search, if open, is out of date.
    pub(super) fn search_output(&mut self, pane: PaneId) {
        if let Some(state) = &mut self.search
            && state.pane == pane
        {
            state.stale = true;
        }
    }

    /// Re-runs a stale search when it's due. Returns when to check again.
    pub(super) fn refresh_search(&mut self, now: Instant) -> Option<Instant> {
        let state = self.search.as_ref().filter(|s| s.stale)?;
        let due = state.last_run + RESEARCH_INTERVAL;
        if now < due {
            return Some(due);
        }
        self.run_search();
        self.window.request_redraw();
        None
    }

    pub(super) fn draw_search(&self, theme: &Theme, out: &mut Vec<ChromeItem>) {
        let Some(state) = &self.search else { return };
        let Some(pane) = self.tabs[self.active_tab].pane(state.pane) else { return };
        let m = self.renderer.cell_metrics();
        let (top, rows) = {
            let term = pane.lock();
            (term.viewport_to_absolute(0), term.rows())
        };
        search::draw_matches(&state.bar, top, rows, pane.origin, theme, m, out);
        if let Some(layout) = self.search_layout() {
            search::draw(&state.bar, &layout, theme, m, self.scale_factor as f32, out);
        }
    }

    /// Hover over the bar. Returns whether the pointer is on it.
    pub(super) fn search_hover(&mut self, x: f32, y: f32) -> bool {
        let Some(layout) = self.search_layout() else { return false };
        let hover = search::hit(&layout, x, y);
        if let Some(state) = &mut self.search
            && state.bar.hover != hover
        {
            state.bar.hover = hover;
            self.window.request_redraw();
        }
        if let Some(target) = hover {
            self.set_cursor_icon(if target == search::Target::Input { CursorIcon::Text } else { CursorIcon::Pointer });
            return true;
        }
        false
    }
}

impl App {
    /// ctrl+shift+f: opens the search on the active pane, focuses it, or
    /// closes it when it already has the keyboard.
    pub(super) fn toggle_search(&mut self) {
        let Some(w) = &mut self.window else { return };
        let pane = w.active_pane().id;
        match &mut w.search {
            Some(state) if state.pane == pane && state.bar.focused => w.search = None,
            Some(state) if state.pane == pane => state.bar.focused = true,
            _ => {
                // The selection, if one line, is a good first query.
                let query = w.active_pane().selected_text().filter(|t| !t.contains('\n')).unwrap_or_default();
                let mut bar = Search::new();
                bar.query.set(query);
                w.search = Some(SearchState {
                    pane,
                    bar,
                    stale: false,
                    last_run: Instant::now(),
                });
                w.run_search();
                w.show_match();
            }
        }
        w.window.request_redraw();
    }

    pub(super) fn search_key(&mut self, event: &KeyEvent) {
        let Some(w) = &mut self.window else { return };
        let mods = w.modifiers;
        let Some(state) = &mut w.search else { return };
        let request = state.bar.key(&event.logical_key, event.text.as_deref(), mods);
        self.search_request(request);
    }

    /// A click on the bar. Returns whether it was on the bar.
    pub(super) fn search_click(&mut self, x: f32, y: f32) -> bool {
        let Some(w) = &mut self.window else { return false };
        let Some(layout) = w.search_layout() else { return false };
        let Some(target) = search::hit(&layout, x, y) else {
            // A click in the pane keeps the highlights, but the keys go back.
            if let Some(state) = &mut w.search {
                state.bar.focused = false;
            }
            return false;
        };
        let (m, scale, extend) = (w.renderer.cell_metrics(), w.scale_factor as f32, w.modifiers.shift_key());
        let Some(state) = &mut w.search else { return false };
        let request = state.bar.click(target);
        if target == search::Target::Input {
            state.bar.query.click((x - search::text_x(&layout, m, scale)) / m.width, extend);
        }
        self.search_request(request);
        true
    }

    fn search_request(&mut self, request: Option<Request>) {
        let Some(w) = &mut self.window else { return };
        w.window.request_redraw();
        match request {
            Some(Request::Search) => {
                w.run_search();
                w.show_match();
            }
            Some(Request::Show) => w.show_match(),
            Some(Request::Close) => w.search = None,
            Some(Request::Unfocus) => {
                if let Some(state) = &mut w.search {
                    state.bar.focused = false;
                }
            }
            None => {}
        }
    }

    /// Pasted text goes into the query while the bar has the keyboard.
    pub(super) fn search_paste(&mut self, text: &str) -> bool {
        let Some(w) = &mut self.window else { return false };
        if !w.search_focused() {
            return false;
        }
        if let Some(state) = &mut w.search {
            state.bar.query.insert(text);
        }
        self.search_request(Some(Request::Search));
        true
    }
}
