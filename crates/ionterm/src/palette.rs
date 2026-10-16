//! A searchable picker drawn over the terminal: the "Connect SSH" palette.

use ionterm_render::{CellMetrics, ChromeItem, text_columns};
use ionterm_vt::Rgb;
use winit::keyboard::{Key, ModifiersState};

use crate::input::Action;
use crate::layout::Rect;
use crate::tabbar::{mix, truncate};
use crate::textfield::{Edit, TextField};
use crate::theme::Theme;

const MAX_VISIBLE_ROWS: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteAction {
    /// Connect to an ssh config host alias.
    Connect(String),
    /// Open a host in the connection editor.
    Edit(String),
    NewConnection,
    /// Run an action (the command palette).
    Command(Action),
}

#[derive(Clone, Debug)]
pub struct PaletteItem {
    pub title: String,
    /// Secondary text, like `user@host:port`.
    pub detail: String,
    pub group: String,
    pub icon: char,
    pub color: Option<Rgb>,
    pub action: PaletteAction,
}

pub struct Palette {
    pub items: Vec<PaletteItem>,
    pub query: TextField,
    /// Position in the filtered list.
    pub selected: usize,
    /// First filtered row shown.
    pub scroll: usize,
    /// Opened from a button: its bottom-left corner. `None` = centred.
    pub anchor: Option<(f32, f32)>,
    /// Filtered row under the mouse.
    pub hover: Option<usize>,
    /// What the search field says while empty.
    pub placeholder: &'static str,
    /// The key hints along the bottom.
    pub footer: &'static str,
}

impl Palette {
    pub fn new(items: Vec<PaletteItem>, anchor: Option<(f32, f32)>) -> Self {
        Self {
            items,
            query: TextField::default(),
            selected: 0,
            scroll: 0,
            anchor,
            hover: None,
            placeholder: "Connect to an SSH host…",
            footer: "↵ connect   ctrl+↵ split   ctrl+e edit   esc close",
        }
    }

    /// The command palette: actions first, then hosts to connect to.
    pub fn commands(items: Vec<PaletteItem>) -> Self {
        Self {
            placeholder: "Run a command, or connect to a host…",
            footer: "↵ run   esc close",
            ..Self::new(items, None)
        }
    }

    /// Item indices matching the query, best first. Items that aren't hosts
    /// (like "New connection…") always stay at the end.
    pub fn matches(&self) -> Vec<usize> {
        let mut scored: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| matches!(item.action, PaletteAction::Connect(_) | PaletteAction::Command(_)))
            .filter_map(|(i, item)| {
                let haystack = format!("{} {} {}", item.title, item.detail, item.group);
                fuzzy_score(self.query.text(), &haystack, item.title.len()).map(|score| (score, i))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut out: Vec<usize> = scored.into_iter().map(|(_, i)| i).collect();
        out.extend(
            self.items
                .iter()
                .enumerate()
                .filter(|(_, item)| !matches!(item.action, PaletteAction::Connect(_) | PaletteAction::Command(_)))
                .map(|(i, _)| i),
        );
        out
    }

    pub fn selected_action(&self) -> Option<PaletteAction> {
        let matches = self.matches();
        matches.get(self.selected).map(|&i| self.items[i].action.clone())
    }

    pub fn move_selection(&mut self, delta: isize) {
        let count = self.matches().len();
        if count == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, count as isize - 1) as usize;
        self.keep_selection_visible();
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let count = self.matches().len();
        let max = count.saturating_sub(MAX_VISIBLE_ROWS);
        self.scroll = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
    }

    fn keep_selection_visible(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + MAX_VISIBLE_ROWS {
            self.scroll = self.selected + 1 - MAX_VISIBLE_ROWS;
        }
    }

    pub fn type_text(&mut self, text: &str) {
        self.query.insert(text);
        self.query_changed();
    }

    /// Feeds an editing key to the search field.
    pub fn edit(&mut self, key: &Key, text: Option<&str>, mods: ModifiersState) {
        if self.query.key(key, text, mods) == Some(Edit::Changed) {
            self.query_changed();
        }
    }

    /// The query changed: back to the best match.
    fn query_changed(&mut self) {
        self.selected = 0;
        self.scroll = 0;
    }
}

/// Scores `query` as a subsequence of `text`, case-insensitively. Matches at
/// word starts and runs of consecutive matches score higher, and so do
/// matches in the first `title_len` bytes. `None` = no match.
fn fuzzy_score(query: &str, text: &str, title_len: usize) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let text_lower: Vec<(usize, char)> = text.char_indices().map(|(i, c)| (i, c.to_ascii_lowercase())).collect();
    let mut score = 0;
    let mut position = 0;
    let mut previous: Option<usize> = None;
    for q in query.chars().map(|c| c.to_ascii_lowercase()) {
        let found = text_lower[position..].iter().position(|&(_, c)| c == q)? + position;
        let (byte, _) = text_lower[found];
        score += 1;
        if previous == Some(found.wrapping_sub(1)) {
            score += 5;
        }
        let at_word_start = found == 0 || matches!(text_lower[found - 1].1, ' ' | '-' | '_' | '.' | '@');
        if at_word_start {
            score += 3;
        }
        if byte < title_len {
            score += 2;
        }
        previous = Some(found);
        position = found + 1;
    }
    Some(score)
}

pub struct PaletteLayout {
    pub panel: Rect,
    pub input: Rect,
    /// (filtered position, item index, row rect) for the visible rows.
    pub rows: Vec<(usize, usize, Rect)>,
    pub footer_y: f32,
}

pub fn layout(palette: &Palette, m: CellMetrics, scale: f32, window: Rect) -> PaletteLayout {
    let px = |v: f32| (v * scale).round();
    let pad = px(8.0);
    let width = (m.width * 64.0).min(window.width - px(24.0)).max(m.width * 20.0);
    let input_h = (m.height * 1.9).round();
    let row_h = (m.height * 1.6).round();
    let matches = palette.matches();
    let visible = matches.len().min(MAX_VISIBLE_ROWS);
    let footer_h = (m.height * 1.4).round();
    let height = pad + input_h + px(6.0) + visible as f32 * row_h + footer_h + pad;

    let (x, y) = match palette.anchor {
        Some((ax, ay)) => (ax, ay + px(4.0)),
        None => (window.x + (window.width - width) / 2.0, window.y + (window.height * 0.12).round()),
    };
    let x = x.min(window.x + window.width - width - px(8.0)).max(window.x + px(8.0)).round();
    let y = y.min(window.y + window.height - height).max(window.y).round();
    let panel = Rect::new(x, y, width, height);
    let input = Rect::new(x + pad, y + pad, width - 2.0 * pad, input_h);
    let list_y = input.y + input_h + px(6.0);
    let rows = matches
        .iter()
        .enumerate()
        .skip(palette.scroll)
        .take(visible)
        .enumerate()
        .map(|(slot, (position, &item))| (position, item, Rect::new(x + pad, list_y + slot as f32 * row_h, width - 2.0 * pad, row_h)))
        .collect();
    PaletteLayout {
        panel,
        input,
        rows,
        footer_y: list_y + visible as f32 * row_h + ((footer_h - m.height) / 2.0).round(),
    }
}

/// The filtered row under a point. `Some(None)` = inside the palette but on
/// no row; `None` = outside it.
pub fn hit(layout: &PaletteLayout, x: f32, y: f32) -> Option<Option<usize>> {
    if !layout.panel.contains(x, y) {
        return None;
    }
    Some(layout.rows.iter().find(|(_, _, r)| r.contains(x, y)).map(|(position, _, _)| *position))
}

pub fn draw(palette: &Palette, layout: &PaletteLayout, theme: &Theme, m: CellMetrics, scale: f32, out: &mut Vec<ChromeItem>) {
    let bg = theme.background.0;
    let fg = theme.foreground.0;
    let panel_bg = mix(bg, fg, 0.08);
    let muted = mix(fg, panel_bg, 0.45);
    let accent = theme.cursor.0;
    let p = layout.panel;
    let edge = scale.round().max(1.0);
    let radius = (10.0 * scale).round();
    out.push(ChromeItem::RoundedRect {
        rect: [p.x - edge, p.y - edge, p.width + 2.0 * edge, p.height + 2.0 * edge],
        radius: radius + edge,
        top_only: false,
        color: mix(bg, fg, 0.2),
        alpha: 1.0,
    });
    out.push(ChromeItem::RoundedRect {
        rect: [p.x, p.y, p.width, p.height],
        radius,
        top_only: false,
        color: panel_bg,
        alpha: 1.0,
    });

    // Search field.
    let i = layout.input;
    out.push(ChromeItem::RoundedRect {
        rect: [i.x, i.y, i.width, i.height],
        radius: (6.0 * scale).round(),
        top_only: false,
        color: mix(bg, fg, 0.03),
        alpha: 1.0,
    });
    let text_y = i.y + ((i.height - m.height) / 2.0).round();
    let text_x = i.x + (10.0 * scale).round();
    out.push(ChromeItem::Text {
        text: "\u{F002}".into(),
        x: text_x,
        y: text_y,
        color: muted,
        bold: false,
    });
    let query_x = text_x + 2.0 * m.width;
    let query_cols = ((i.width - (query_x - i.x) - m.width) / m.width).max(1.0) as usize;
    if palette.query.is_empty() {
        out.push(ChromeItem::Text {
            text: palette.placeholder.into(),
            x: query_x,
            y: text_y,
            color: muted,
            bold: false,
        });
    }
    palette.query.draw(query_x, text_y, query_cols, fg, accent, true, m, scale, out);

    // Rows.
    for &(position, item_index, r) in &layout.rows {
        let item = &palette.items[item_index];
        let selected = position == palette.selected;
        let hovered = palette.hover == Some(position);
        if selected || hovered {
            out.push(ChromeItem::RoundedRect {
                rect: [r.x, r.y, r.width, r.height],
                radius: (6.0 * scale).round(),
                top_only: false,
                color: mix(panel_bg, fg, if selected { 0.12 } else { 0.06 }),
                alpha: 1.0,
            });
        }
        if selected {
            out.push(ChromeItem::RoundedRect {
                rect: [r.x, r.y + (r.height * 0.25).round(), (3.0 * scale).round(), (r.height * 0.5).round()],
                radius: (1.5 * scale).round(),
                top_only: false,
                color: accent,
                alpha: 1.0,
            });
        }
        let y = r.y + ((r.height - m.height) / 2.0).round();
        let x = r.x + (12.0 * scale).round();
        out.push(ChromeItem::Text {
            text: item.icon.to_string(),
            x,
            y,
            color: item.color.unwrap_or(muted),
            bold: false,
        });
        let total_cols = ((r.width - (x - r.x) - 2.0 * m.width) / m.width) as usize;
        let group_cols = text_columns(&item.group).min(total_cols / 4);
        let title = truncate(&item.title, total_cols.saturating_sub(group_cols + 3).max(4));
        let title_cols = text_columns(&title);
        out.push(ChromeItem::Text {
            text: title,
            x: x + 2.0 * m.width,
            y,
            color: fg,
            bold: false,
        });
        let detail_cols = total_cols.saturating_sub(title_cols + group_cols + 5);
        if detail_cols > 3 && !item.detail.is_empty() {
            out.push(ChromeItem::Text {
                text: truncate(&item.detail, detail_cols),
                x: x + (title_cols + 4) as f32 * m.width,
                y,
                color: muted,
                bold: false,
            });
        }
        if group_cols > 0 {
            let group = truncate(&item.group, group_cols);
            out.push(ChromeItem::Text {
                x: r.x + r.width - (text_columns(&group) as f32 + 1.0) * m.width,
                text: group,
                y,
                color: muted,
                bold: false,
            });
        }
    }

    out.push(ChromeItem::Text {
        text: palette.footer.into(),
        x: p.x + (14.0 * scale).round(),
        y: layout.footer_y,
        color: muted,
        bold: false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, detail: &str) -> PaletteItem {
        PaletteItem {
            title: title.into(),
            detail: detail.into(),
            group: String::new(),
            icon: ' ',
            color: None,
            action: PaletteAction::Connect(title.into()),
        }
    }

    #[test]
    fn filters_and_ranks() {
        let mut items = vec![item("database", "db.internal"), item("hostion", "ion@95.216.26.138"), item("web-prod", "deploy@web")];
        items.push(PaletteItem {
            action: PaletteAction::NewConnection,
            ..item("New connection…", "")
        });
        let mut p = Palette::new(items, None);
        assert_eq!(p.matches(), [0, 1, 2, 3]);
        p.type_text("hos");
        assert_eq!(p.matches(), [1, 3]);
        p.query.set("wp");
        assert_eq!(p.matches()[0], 2);
        p.move_selection(5);
        assert_eq!(p.selected_action(), Some(PaletteAction::NewConnection));
    }
}
