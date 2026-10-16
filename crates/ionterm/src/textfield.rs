//! A one-line text field: the text, a caret and a selection, with the usual
//! editing keys. Every input ionterm draws (forms, search, pickers, the tab
//! rename) keeps its text in one of these.

use std::cell::Cell;
use std::ops::Range;

use ionterm_render::{CellMetrics, ChromeItem, text_columns};
use ionterm_vt::Rgb;
use unicode_width::UnicodeWidthChar;
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// What a key did to the field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    /// The caret or selection moved; the text is the same.
    Moved,
    Changed,
}

#[derive(Clone, Debug, Default)]
pub struct TextField {
    text: String,
    /// Byte offset of the caret.
    cursor: usize,
    /// The selection's other end; equal to `cursor` when nothing is selected.
    anchor: usize,
    /// First column shown, kept by drawing so the caret stays in view.
    scroll: Cell<usize>,
    /// A password: shown as dots, and never copied.
    secret: bool,
}

/// What a field shows in a given width.
#[derive(Debug, PartialEq, Eq)]
pub struct View {
    pub text: String,
    /// The caret's column within `text`.
    pub caret: usize,
    /// Selected columns within `text`, start before end.
    pub selection: Option<(usize, usize)>,
}

/// Whether a character joins words, separates them, or is punctuation, for
/// ctrl+arrow jumps and ctrl+backspace.
#[derive(PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punctuation,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punctuation
    }
}

impl TextField {
    /// A field holding `text`, with the caret at its end.
    pub fn new(text: impl Into<String>) -> Self {
        let mut field = Self::default();
        field.set(text);
        field
    }

    /// A field for a password.
    pub fn secret() -> Self {
        Self {
            secret: true,
            ..Self::default()
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Columns a character takes as shown (a password's dots take one).
    fn width_of(&self, c: char) -> usize {
        if self.secret { 1 } else { c.width().unwrap_or(0) }
    }

    fn columns(&self, text: &str) -> usize {
        if self.secret { text.chars().count() } else { text_columns(text) }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replaces the text, with the caret at the end.
    pub fn set(&mut self, text: impl Into<String>) {
        self.text = first_line(&text.into()).to_string();
        self.cursor = self.text.len();
        self.anchor = self.cursor;
    }

    /// The selected byte range, if anything is selected.
    pub fn selection(&self) -> Option<Range<usize>> {
        (self.cursor != self.anchor).then(|| self.cursor.min(self.anchor)..self.cursor.max(self.anchor))
    }

    pub fn selected_text(&self) -> Option<&str> {
        self.selection().filter(|_| !self.secret).map(|range| &self.text[range])
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
    }

    /// Types or pastes `text` at the caret, replacing the selection. Only the
    /// first line is kept: a field is one line.
    pub fn insert(&mut self, text: &str) {
        let text = first_line(text);
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        self.text.replace_range(range.clone(), text);
        self.cursor = range.start + text.len();
        self.anchor = self.cursor;
    }

    /// Removes the selection, returning whether there was one.
    fn delete_selection(&mut self) -> bool {
        let Some(range) = self.selection() else { return false };
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = self.cursor;
        true
    }

    fn previous_boundary(&self, from: usize) -> usize {
        self.text[..from].char_indices().next_back().map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, from: usize) -> usize {
        self.text[from..].chars().next().map_or(from, |c| from + c.len_utf8())
    }

    /// The start of the word before `from`, skipping spaces first.
    fn previous_word(&self, from: usize) -> usize {
        let mut chars = self.text[..from].char_indices().rev().peekable();
        while chars.next_if(|(_, c)| class(*c) == Class::Space).is_some() {}
        let Some(kind) = chars.peek().map(|(_, c)| class(*c)) else { return 0 };
        let mut start = 0;
        while let Some((i, _)) = chars.next_if(|(_, c)| class(*c) == kind) {
            start = i;
        }
        start
    }

    /// The end of the word after `from`, and the spaces after it.
    fn next_word(&self, from: usize) -> usize {
        let mut chars = self.text[from..].char_indices().map(|(i, c)| (from + i, c)).peekable();
        if let Some(kind) = chars.peek().map(|(_, c)| class(*c)).filter(|k| *k != Class::Space) {
            while chars.next_if(|(_, c)| class(*c) == kind).is_some() {}
        }
        while chars.next_if(|(_, c)| class(*c) == Class::Space).is_some() {}
        chars.peek().map_or(self.text.len(), |(i, _)| *i)
    }

    /// Moves the caret, extending the selection when `extend`.
    fn move_to(&mut self, to: usize, extend: bool) {
        self.cursor = to;
        if !extend {
            self.anchor = to;
        }
    }

    /// Handles an editing key: caret movement, selection, deleting and
    /// typing. Returns `None` for keys that aren't the field's (Enter,
    /// Escape, Tab, up and down…), so the owner can use them.
    /// The clipboard keys are the app's, which owns the clipboard.
    pub fn key(&mut self, key: &Key, text: Option<&str>, mods: ModifiersState) -> Option<Edit> {
        let (ctrl, shift) = (mods.control_key(), mods.shift_key());
        let edit = match key {
            Key::Named(NamedKey::ArrowLeft) => {
                let to = match self.selection() {
                    Some(range) if !shift && !ctrl => range.start,
                    _ if ctrl => self.previous_word(self.cursor),
                    _ => self.previous_boundary(self.cursor),
                };
                self.move_to(to, shift);
                Edit::Moved
            }
            Key::Named(NamedKey::ArrowRight) => {
                let to = match self.selection() {
                    Some(range) if !shift && !ctrl => range.end,
                    _ if ctrl => self.next_word(self.cursor),
                    _ => self.next_boundary(self.cursor),
                };
                self.move_to(to, shift);
                Edit::Moved
            }
            Key::Named(NamedKey::Home) => {
                self.move_to(0, shift);
                Edit::Moved
            }
            Key::Named(NamedKey::End) => {
                self.move_to(self.text.len(), shift);
                Edit::Moved
            }
            Key::Named(NamedKey::Backspace) => {
                if !self.delete_selection() {
                    let from = if ctrl { self.previous_word(self.cursor) } else { self.previous_boundary(self.cursor) };
                    if from == self.cursor {
                        return Some(Edit::Moved);
                    }
                    self.text.replace_range(from..self.cursor, "");
                    self.move_to(from, false);
                }
                Edit::Changed
            }
            Key::Named(NamedKey::Delete) => {
                if !self.delete_selection() {
                    let to = if ctrl { self.next_word(self.cursor) } else { self.next_boundary(self.cursor) };
                    if to == self.cursor {
                        return Some(Edit::Moved);
                    }
                    self.text.replace_range(self.cursor..to, "");
                }
                Edit::Changed
            }
            Key::Character(c) if ctrl && !mods.alt_key() && c.eq_ignore_ascii_case("a") => {
                self.select_all();
                Edit::Moved
            }
            // AltGr comes through as ctrl+alt on some systems.
            _ if !ctrl || mods.alt_key() => {
                let text = text.filter(|t| !t.is_empty() && !t.chars().any(char::is_control))?;
                self.insert(text);
                Edit::Changed
            }
            _ => return None,
        };
        Some(edit)
    }

    /// The text that fits in `cols` columns, scrolled so the caret shows.
    pub fn view(&self, cols: usize) -> View {
        let cols = cols.max(1);
        let caret = self.columns(&self.text[..self.cursor]);
        let total = self.columns(&self.text);
        // One column is kept for the caret after the last character.
        let mut scroll = self.scroll.get().min((total + 1).saturating_sub(cols));
        if caret < scroll {
            scroll = caret;
        } else if caret >= scroll + cols {
            scroll = caret + 1 - cols;
        }
        self.scroll.set(scroll);

        let selection = self.selection();
        let mut shown = String::new();
        let (mut col, mut first, mut selected) = (0, None, None::<(usize, usize)>);
        for (i, c) in self.text.char_indices() {
            let width = self.width_of(c);
            if col >= scroll && col + width <= scroll + cols {
                let start = *first.get_or_insert(col);
                shown.push(if self.secret { '•' } else { c });
                if selection.as_ref().is_some_and(|r| r.contains(&i)) {
                    let at = col - start;
                    selected = Some(selected.map_or((at, at + width), |(s, _)| (s, at + width)));
                }
            }
            col += width;
        }
        let start = first.unwrap_or(scroll);
        View {
            text: shown,
            caret: caret.saturating_sub(start).min(cols),
            selection: selected,
        }
    }

    /// A click at `col` columns (fractional) from where the text is drawn:
    /// moves the caret to the nearest boundary, or extends the selection.
    pub fn click(&mut self, col: f32, extend: bool) {
        let target = self.scroll.get() as f32 + col.max(0.0);
        let mut at = 0.0;
        let mut to = self.text.len();
        for (i, c) in self.text.char_indices() {
            let width = self.width_of(c) as f32;
            if target < at + width / 2.0 {
                to = i;
                break;
            }
            at += width;
        }
        self.move_to(to, extend);
    }

    /// Draws the text at (`x`, `y`) in `cols` columns; when `focused`, also
    /// the selection and the caret, in `accent`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&self, x: f32, y: f32, cols: usize, color: Rgb, accent: Rgb, focused: bool, m: CellMetrics, scale: f32, out: &mut Vec<ChromeItem>) {
        let view = self.view(cols);
        if focused && let Some((start, end)) = view.selection {
            out.push(ChromeItem::Rect {
                rect: [x + start as f32 * m.width, y, (end - start) as f32 * m.width, m.height],
                color: accent,
                alpha: 0.35,
            });
        }
        let caret = view.caret;
        out.push(ChromeItem::Text {
            text: view.text,
            x,
            y,
            color,
            bold: false,
        });
        if focused {
            out.push(ChromeItem::Rect {
                rect: [x + caret as f32 * m.width, y, (2.0 * scale).round(), m.height],
                color: accent,
                alpha: 1.0,
            });
        }
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(field: &mut TextField, key: Key, mods: ModifiersState) -> Option<Edit> {
        field.key(&key, None, mods)
    }

    fn named(key: NamedKey) -> Key {
        Key::Named(key)
    }

    #[test]
    fn moves_and_edits_in_the_middle() {
        let mut field = TextField::new("hello world");
        press(&mut field, named(NamedKey::Home), ModifiersState::empty());
        press(&mut field, named(NamedKey::ArrowRight), ModifiersState::empty());
        field.key(&Key::Character("X".into()), Some("X"), ModifiersState::empty());
        assert_eq!(field.text(), "hXello world");
        press(&mut field, named(NamedKey::Delete), ModifiersState::empty());
        assert_eq!(field.text(), "hXllo world");
        press(&mut field, named(NamedKey::Backspace), ModifiersState::empty());
        assert_eq!(field.text(), "hllo world");
        assert_eq!(press(&mut field, named(NamedKey::Enter), ModifiersState::empty()), None);
    }

    #[test]
    fn words_and_selection() {
        let mut field = TextField::new("ssh user@host.example -p 22");
        press(&mut field, named(NamedKey::ArrowLeft), ModifiersState::CONTROL);
        assert_eq!(&field.text()[field.cursor..], "22");
        press(&mut field, named(NamedKey::ArrowLeft), ModifiersState::CONTROL | ModifiersState::SHIFT);
        assert_eq!(field.selected_text(), Some("p "));
        press(&mut field, named(NamedKey::Backspace), ModifiersState::empty());
        assert_eq!(field.text(), "ssh user@host.example -22");
        press(&mut field, named(NamedKey::Backspace), ModifiersState::CONTROL);
        assert_eq!(field.text(), "ssh user@host.example 22");
        press(&mut field, named(NamedKey::Backspace), ModifiersState::CONTROL);
        assert_eq!(field.text(), "ssh user@host.22");
        press(&mut field, named(NamedKey::Home), ModifiersState::empty());
        press(&mut field, named(NamedKey::ArrowRight), ModifiersState::CONTROL);
        assert_eq!(&field.text()[field.cursor..], "user@host.22");
        press(&mut field, named(NamedKey::Delete), ModifiersState::CONTROL);
        assert_eq!(field.text(), "ssh @host.22");

        field.key(&Key::Character("a".into()), Some("\u{1}"), ModifiersState::CONTROL);
        assert_eq!(field.selected_text(), Some("ssh @host.22"));
        field.insert("new\nignored");
        assert_eq!(field.text(), "new");
        // Shift+arrow then a plain arrow collapses to that side.
        press(&mut field, named(NamedKey::Home), ModifiersState::SHIFT);
        press(&mut field, named(NamedKey::ArrowRight), ModifiersState::empty());
        assert_eq!((field.cursor, field.selection()), (3, None));
    }

    #[test]
    fn scrolls_to_keep_the_caret_shown() {
        let mut field = TextField::new("abcdefghij");
        let view = field.view(5);
        assert_eq!((view.text.as_str(), view.caret), ("ghij", 4));
        press(&mut field, named(NamedKey::Home), ModifiersState::empty());
        let view = field.view(5);
        assert_eq!((view.text.as_str(), view.caret), ("abcde", 0));
        field.click(2.4, false);
        assert_eq!(field.cursor, 2);
        field.click(4.6, true);
        assert_eq!(field.selected_text(), Some("cde"));
        // The caret after "e" needs a column: the view scrolls by one.
        assert_eq!(field.view(5).selection, Some((1, 4)));
    }
}
