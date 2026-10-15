//! Scrollback search (ctrl+shift+f): a small bar over the top-right of the
//! active pane that finds text in the whole scrollback, plain or regex,
//! highlights every match on screen and steps between them.
//!
//! Lines that soft-wrapped are searched as one, so a match can span rows.
//! Smart case: all-lowercase queries ignore case, any capital makes it
//! exact.

use ionterm_render::{CellMetrics, ChromeItem, text_columns};
use ionterm_vt::{Flags, Rgb, Term};
use regex::{Regex, RegexBuilder};
use winit::keyboard::{Key, NamedKey};

use crate::layout::Rect;
use crate::tabbar::{mix, truncate};
use crate::theme::Theme;

/// A match: first and last character, as (absolute line, column).
pub type Match = ((usize, usize), (usize, usize));

/// Compiles the query: escaped unless `regex`, case-insensitive unless it
/// has a capital letter.
pub fn compile(query: &str, regex: bool) -> Result<Regex, String> {
    let pattern = if regex { query.to_string() } else { regex::escape(query) };
    let exact = query.chars().any(char::is_uppercase);
    RegexBuilder::new(&pattern)
        .case_insensitive(!exact)
        .build()
        .map_err(|e| e.to_string().lines().last().unwrap_or("Invalid pattern").trim().to_string())
}

/// Every match in the terminal, oldest first.
pub fn find_all(term: &Term, pattern: &Regex) -> Vec<Match> {
    let total = term.history_len() + term.rows();
    let mut matches = Vec::new();
    let mut line = 0;
    let mut text = String::new();
    // Each character's position, by its byte offset in `text`.
    let mut positions: Vec<(usize, usize)> = Vec::new();
    let mut by_byte: Vec<u32> = Vec::new();
    while line < total {
        text.clear();
        positions.clear();
        by_byte.clear();
        // A logical line: rows joined where they soft-wrapped.
        while let Some(row) = term.line(line) {
            for (col, cell) in row.cells.iter().enumerate() {
                if cell.flags.contains(Flags::WIDE_SPACER) {
                    continue;
                }
                let index = positions.len() as u32;
                positions.push((line, col));
                let before = text.len();
                text.push(cell.c);
                by_byte.extend(std::iter::repeat_n(index, text.len() - before));
            }
            line += 1;
            if !row.wrapped {
                break;
            }
        }
        for found in pattern.find_iter(&text) {
            if found.start() == found.end() {
                continue;
            }
            let first = by_byte[found.start()] as usize;
            let last = by_byte[found.end() - 1] as usize;
            matches.push((positions[first], positions[last]));
        }
    }
    matches
}

pub struct Search {
    pub query: String,
    pub regex: bool,
    pub matches: Vec<Match>,
    /// The match shown, an index into `matches`.
    pub current: Option<usize>,
    pub error: Option<String>,
    /// The bar has the keyboard (it can stay open while you type in the
    /// pane).
    pub focused: bool,
    pub hover: Option<Target>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Input,
    Previous,
    Next,
    Regex,
    Close,
}

/// What the bar asks the window to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    /// The query changed: search again.
    Search,
    /// Show the match `matches[current]`.
    Show,
    Close,
    /// Give the keyboard back to the pane, keeping the highlights.
    Unfocus,
}

impl Default for Search {
    fn default() -> Self {
        Self::new()
    }
}

impl Search {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            regex: false,
            matches: Vec::new(),
            current: None,
            error: None,
            focused: true,
            hover: None,
        }
    }

    /// Takes new results, keeping the current match where it was if it's
    /// still there, else the newest one at or above `near` (the line at the
    /// bottom of the view).
    pub fn set_matches(&mut self, matches: Vec<Match>, near: usize) {
        let previous = self.current.and_then(|i| self.matches.get(i)).copied();
        self.current = previous
            .and_then(|m| matches.iter().position(|x| *x == m))
            .or_else(|| matches.iter().rposition(|m| m.0.0 <= near))
            .or(if matches.is_empty() { None } else { Some(matches.len() - 1) });
        self.matches = matches;
    }

    /// Steps to the previous (older, up) or next (newer, down) match.
    pub fn step(&mut self, older: bool) -> Option<Request> {
        let count = self.matches.len();
        if count == 0 {
            return None;
        }
        self.current = Some(match (self.current, older) {
            (Some(i), true) => (i + count - 1) % count,
            (Some(i), false) => (i + 1) % count,
            (None, _) => count - 1,
        });
        Some(Request::Show)
    }

    pub fn key(&mut self, key: &Key, text: Option<&str>, shift: bool, alt: bool) -> Option<Request> {
        match key {
            Key::Named(NamedKey::Escape) => return Some(Request::Close),
            // Enter steps up (back in time), shift+Enter down.
            Key::Named(NamedKey::Enter) => return self.step(!shift),
            Key::Named(NamedKey::ArrowUp) => return self.step(true),
            Key::Named(NamedKey::ArrowDown) => return self.step(false),
            Key::Named(NamedKey::Tab) => return Some(Request::Unfocus),
            Key::Named(NamedKey::Backspace) => {
                self.query.pop();
                return Some(Request::Search);
            }
            Key::Character(c) if alt && c.eq_ignore_ascii_case("r") => {
                self.regex = !self.regex;
                return Some(Request::Search);
            }
            _ => {}
        }
        let text = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control))?;
        self.query.push_str(text);
        Some(Request::Search)
    }

    pub fn click(&mut self, target: Target) -> Option<Request> {
        match target {
            Target::Input => {
                self.focused = true;
                None
            }
            Target::Previous => self.step(true),
            Target::Next => self.step(false),
            Target::Regex => {
                self.regex = !self.regex;
                Some(Request::Search)
            }
            Target::Close => Some(Request::Close),
        }
    }

    /// "3 of 17", "No matches", or the pattern's error.
    fn status(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        match (self.current, self.matches.len()) {
            (_, 0) if self.query.is_empty() => String::new(),
            (_, 0) => "No matches".into(),
            (Some(i), n) => format!("{} of {n}", i + 1),
            (None, n) => format!("{n} found"),
        }
    }
}

pub struct SearchLayout {
    pub bar: Rect,
    pub input: Rect,
    pub buttons: Vec<(Target, Rect)>,
}

/// The bar, at the top right of `pane`.
pub fn layout(pane: Rect, m: CellMetrics, scale: f32) -> SearchLayout {
    let px = |v: f32| (v * scale).round();
    let height = (m.height * 1.9).round();
    let button = height - px(8.0);
    let width = (m.width * 52.0).min(pane.width - px(16.0)).max(m.width * 24.0);
    let bar = Rect::new(pane.x + pane.width - width - px(10.0), pane.y + px(8.0), width, height);
    let mut x = bar.x + bar.width - px(4.0);
    let mut buttons = Vec::new();
    for target in [Target::Close, Target::Next, Target::Previous, Target::Regex] {
        x -= button;
        buttons.push((target, Rect::new(x, bar.y + px(4.0), button, button)));
        x -= px(2.0);
    }
    let input = Rect::new(bar.x + px(4.0), bar.y + px(4.0), x - bar.x - px(8.0), button);
    SearchLayout { bar, input, buttons }
}

pub fn hit(layout: &SearchLayout, x: f32, y: f32) -> Option<Target> {
    if !layout.bar.contains(x, y) {
        return None;
    }
    Some(layout.buttons.iter().find(|(_, r)| r.contains(x, y)).map_or(Target::Input, |(t, _)| *t))
}

/// Highlights on the pane: every visible match, the current one stronger.
/// `top` is the absolute line at the top of the view.
pub fn draw_matches(search: &Search, top: usize, rows: usize, origin: [f32; 2], theme: &Theme, m: CellMetrics, out: &mut Vec<ChromeItem>) {
    let accent = theme.cursor.0;
    let found = Rgb::new(0xe5, 0xc0, 0x7b);
    for (i, &(start, end)) in search.matches.iter().enumerate() {
        if end.0 < top || start.0 >= top + rows {
            continue;
        }
        let current = search.current == Some(i);
        for line in start.0.max(top)..=end.0.min(top + rows - 1) {
            let first = if line == start.0 { start.1 } else { 0 };
            let last = if line == end.0 { end.1 } else { usize::MAX / 2 };
            let width = (last.saturating_sub(first) + 1).min(10_000) as f32 * m.width;
            let x = origin[0] + first as f32 * m.width;
            let y = origin[1] + (line - top) as f32 * m.height;
            out.push(ChromeItem::Rect {
                rect: [x, y, width.min(10_000.0), m.height],
                color: if current { accent } else { found },
                alpha: if current { 0.55 } else { 0.3 },
            });
            if current {
                out.push(ChromeItem::Rect {
                    rect: [x, y + m.height - 2.0, width, 2.0],
                    color: accent,
                    alpha: 1.0,
                });
            }
        }
    }
}

pub fn draw(search: &Search, layout: &SearchLayout, theme: &Theme, m: CellMetrics, scale: f32, out: &mut Vec<ChromeItem>) {
    let px = |v: f32| (v * scale).round();
    let bg = theme.background.0;
    let fg = theme.foreground.0;
    let panel = mix(bg, fg, 0.09);
    let muted = mix(fg, panel, 0.45);
    let accent = theme.cursor.0;
    let rounded = |r: Rect, radius: f32, color: Rgb| ChromeItem::RoundedRect {
        rect: [r.x, r.y, r.width, r.height],
        radius,
        top_only: false,
        color,
        alpha: 1.0,
    };
    let b = layout.bar;
    out.push(rounded(Rect::new(b.x - px(1.0), b.y - px(1.0), b.width + px(2.0), b.height + px(2.0)), px(9.0), if search.focused { accent } else { mix(bg, fg, 0.22) }));
    out.push(rounded(b, px(8.0), panel));

    let input = layout.input;
    let y = input.y + ((input.height - m.height) / 2.0).round();
    let status = search.status();
    let status_cols = text_columns(&status);
    let icon_x = input.x + px(6.0);
    out.push(ChromeItem::Text {
        text: "\u{F0349}".into(),
        x: icon_x,
        y,
        color: muted,
        bold: false,
    });
    let text_x = icon_x + 2.0 * m.width;
    let cols = (((input.x + input.width - text_x) / m.width) as usize).saturating_sub(status_cols + 2);
    let (shown, color) = if search.query.is_empty() {
        (truncate("Search", cols), mix(muted, panel, 0.3))
    } else {
        let chars: Vec<char> = search.query.chars().collect();
        (chars[chars.len().saturating_sub(cols.saturating_sub(1))..].iter().collect(), fg)
    };
    let shown_cols = if search.query.is_empty() { 0 } else { text_columns(&shown) };
    out.push(ChromeItem::Text {
        text: shown,
        x: text_x,
        y,
        color,
        bold: false,
    });
    if search.focused {
        out.push(ChromeItem::Rect {
            rect: [text_x + shown_cols as f32 * m.width, y, px(2.0), m.height],
            color: accent,
            alpha: 1.0,
        });
    }
    out.push(ChromeItem::Text {
        text: status,
        x: input.x + input.width - (status_cols as f32 + 0.5) * m.width,
        y,
        color: if search.error.is_some() { Rgb::new(0xe0, 0x6c, 0x75) } else { muted },
        bold: false,
    });

    for (target, r) in &layout.buttons {
        let on = *target == Target::Regex && search.regex;
        if on || search.hover == Some(*target) {
            out.push(rounded(*r, px(5.0), if on { mix(panel, accent, 0.45) } else { mix(panel, fg, 0.12) }));
        }
        let glyph = match target {
            Target::Previous => "\u{F0143}",
            Target::Next => "\u{F0140}",
            Target::Regex => ".*",
            Target::Close => "×",
            Target::Input => "",
        };
        let cols = text_columns(glyph) as f32;
        out.push(ChromeItem::Text {
            text: glyph.into(),
            x: r.x + ((r.width - cols * m.width) / 2.0).round(),
            y: r.y + ((r.height - m.height) / 2.0).round(),
            color: if on { fg } else { muted },
            bold: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ionterm_vt::{Palette, Processor};

    fn term_with(text: &[u8], cols: usize) -> Term {
        let mut term = Term::new(cols, 4, 100, Palette::default());
        Processor::new().advance(&mut term, text);
        term
    }

    #[test]
    fn finds_across_wraps_and_scrollback() {
        // Six lines on a four-row screen: two went to the scrollback.
        let term = term_with(b"error one\r\nok\r\nERROR two\r\nok\r\nthe long errorline\r\nend", 10);
        let all = find_all(&term, &compile("error", false).unwrap());
        assert_eq!(all.len(), 3);
        // Smart case: a capital makes it exact.
        assert_eq!(find_all(&term, &compile("ERROR", false).unwrap()).len(), 1);
        // "the long errorline" wraps after 10 columns: "errorline" spans rows.
        let last = all[2];
        assert_eq!((last.0.1, last.1.1), (9, 3));
        assert_eq!(last.1.0, last.0.0 + 1);
    }

    #[test]
    fn regex_and_errors() {
        let term = term_with(b"id=42 id=7 idx=3", 30);
        assert_eq!(find_all(&term, &compile(r"id=\d+", true).unwrap()).len(), 2);
        assert_eq!(find_all(&term, &compile(r"id=\d+", false).unwrap()).len(), 0);
        assert!(compile("(", true).is_err());
    }

    #[test]
    fn steps_wrap_around() {
        let mut search = Search::new();
        search.set_matches(vec![((0, 0), (0, 1)), ((5, 0), (5, 1)), ((9, 0), (9, 1))], 6);
        assert_eq!(search.current, Some(1));
        search.step(true);
        search.step(true);
        assert_eq!(search.current, Some(2));
        search.step(false);
        assert_eq!(search.current, Some(0));
    }
}
