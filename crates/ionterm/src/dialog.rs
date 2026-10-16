//! A small modal dialog drawn over the terminal: a question with buttons,
//! optionally a text field and an "apply to all" checkbox. The file panel
//! uses it to confirm deletes, name new folders and resolve name conflicts.

use ionterm_render::{CellMetrics, ChromeItem, text_columns};
use ionterm_vt::Rgb;
use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::layout::Rect;
use crate::tabbar::{mix, readable_on, truncate};
use crate::textfield::TextField;
use crate::theme::Theme;

#[derive(Clone, Debug)]
pub struct Button {
    pub label: String,
    /// Pressing this letter picks the button (when there is no text field).
    pub key: Option<char>,
    /// Drawn in the theme's red: deleting, overwriting.
    pub danger: bool,
}

impl Button {
    pub fn new(label: &str, key: char) -> Self {
        Self {
            label: label.into(),
            key: Some(key),
            danger: false,
        }
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Button(usize),
    Checkbox,
    Input,
}

/// How the dialog was answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The chosen button; `None` = dismissed (Escape, or a click outside).
    pub button: Option<usize>,
    pub text: String,
    pub checked: bool,
}

pub struct Dialog<P> {
    pub title: String,
    pub message: String,
    /// A text field, with what it holds.
    pub input: Option<TextField>,
    pub buttons: Vec<Button>,
    /// The button Enter picks.
    pub default: usize,
    /// An "apply to all" style checkbox: its label and state.
    pub checkbox: Option<(String, bool)>,
    pub error: Option<String>,
    pub hover: Option<Target>,
    /// What answering it does, for whoever opened it.
    pub purpose: P,
}

impl<P> Dialog<P> {
    pub fn new(title: impl Into<String>, message: impl Into<String>, buttons: Vec<Button>, purpose: P) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            input: None,
            buttons,
            default: 0,
            checkbox: None,
            error: None,
            hover: None,
            purpose,
        }
    }

    pub fn with_input(mut self, text: impl Into<String>) -> Self {
        self.input = Some(TextField::new(text));
        self
    }

    /// A text field for a password: dots, never copied.
    pub fn with_secret_input(mut self) -> Self {
        self.input = Some(TextField::secret());
        self
    }

    pub fn with_checkbox(mut self, label: impl Into<String>) -> Self {
        self.checkbox = Some((label.into(), false));
        self
    }

    /// A button's text as drawn: with its key when there's no text field.
    fn button_label(&self, button: &Button) -> String {
        match button.key.filter(|_| self.input.is_none()) {
            Some(key) => format!("{} ({key})", button.label),
            None => button.label.clone(),
        }
    }

    fn answer(&self, button: Option<usize>) -> Answer {
        Answer {
            button,
            text: self.input.as_ref().map(|i| i.text().to_string()).unwrap_or_default(),
            checked: self.checkbox.as_ref().is_some_and(|(_, c)| *c),
        }
    }

    /// Handles a key press; returns the answer once there is one.
    pub fn key(&mut self, key: &Key, text: Option<&str>, mods: ModifiersState) -> Option<Answer> {
        match key {
            Key::Named(NamedKey::Escape) => return Some(self.answer(None)),
            Key::Named(NamedKey::Enter) => return Some(self.answer(Some(self.default))),
            Key::Named(NamedKey::Tab) | Key::Named(NamedKey::ArrowRight) if self.input.is_none() => {
                self.default = (self.default + 1) % self.buttons.len();
            }
            Key::Named(NamedKey::ArrowLeft) if self.input.is_none() => {
                self.default = (self.default + self.buttons.len() - 1) % self.buttons.len();
            }
            _ => match &mut self.input {
                Some(input) => {
                    input.key(key, text, mods);
                }
                None if !mods.control_key() => {
                    let text = text.filter(|t| !t.chars().any(char::is_control))?;
                    let c = text.chars().next()?.to_ascii_lowercase();
                    if let Some(checkbox) = self.checkbox.as_mut().filter(|_| c == 'a') {
                        checkbox.1 = !checkbox.1;
                        return None;
                    }
                    let index = self.buttons.iter().position(|b| b.key == Some(c))?;
                    return Some(self.answer(Some(index)));
                }
                None => {}
            },
        }
        None
    }

    /// Handles a click: `None` = nothing decided yet.
    pub fn click(&mut self, target: Option<Option<Target>>) -> Option<Answer> {
        match target {
            None => Some(self.answer(None)),
            Some(Some(Target::Button(i))) => Some(self.answer(Some(i))),
            Some(Some(Target::Checkbox)) => {
                if let Some(checkbox) = &mut self.checkbox {
                    checkbox.1 = !checkbox.1;
                }
                None
            }
            Some(_) => None,
        }
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(input) = &mut self.input {
            input.insert(text);
        }
    }

    /// A click at `x` in the text field puts the caret there, or extends the
    /// selection to it.
    pub fn place_caret(&mut self, layout: &DialogLayout, x: f32, m: CellMetrics, scale: f32, extend: bool) {
        if let (Some(input), Some(r)) = (&mut self.input, layout.input) {
            input.click((x - text_x(r, scale)) / m.width, extend);
        }
    }
}

pub struct DialogLayout {
    pub panel: Rect,
    pub title_y: f32,
    /// Wrapped message lines and their y.
    pub lines: Vec<(String, f32)>,
    pub input: Option<Rect>,
    pub checkbox: Option<Rect>,
    pub error_y: f32,
    pub buttons: Vec<Rect>,
}

pub fn layout<P>(dialog: &Dialog<P>, m: CellMetrics, scale: f32, window: Rect) -> DialogLayout {
    let px = |v: f32| (v * scale).round();
    let pad = px(18.0);
    let width = (m.width * 56.0).min(window.width - px(32.0)).max(m.width * 24.0).round();
    let cols = ((width - 2.0 * pad) / m.width).max(8.0) as usize;
    let line_h = (m.height * 1.25).round();
    let row = (m.height * 1.8).round();
    let x = (window.x + (window.width - width) / 2.0).round();
    let mut y = px(0.0);
    let title_y = y + pad;
    y = title_y + m.height + px(10.0);
    let wrapped = wrap(&dialog.message, cols);
    let message_lines: Vec<(String, f32)> = wrapped
        .into_iter()
        .take(8)
        .enumerate()
        .map(|(i, line)| (line, y + i as f32 * line_h))
        .collect();
    y += message_lines.len() as f32 * line_h + px(6.0);
    let input = dialog.input.as_ref().map(|_| {
        let r = Rect::new(x + pad, y, width - 2.0 * pad, row);
        y += row + px(8.0);
        r
    });
    let checkbox = dialog.checkbox.as_ref().map(|_| {
        let r = Rect::new(x + pad, y, width - 2.0 * pad, line_h);
        y += line_h + px(6.0);
        r
    });
    let error_y = y;
    if dialog.error.is_some() {
        y += line_h;
    }
    y += px(6.0);
    let button_w = |label: &str| (text_columns(label) as f32 + 4.0) * m.width;
    let mut right = x + width - pad;
    let mut buttons: Vec<Rect> = dialog
        .buttons
        .iter()
        .rev()
        .map(|b| {
            let w = button_w(&dialog.button_label(b));
            right -= w;
            let r = Rect::new(right, y, w, row);
            right -= px(8.0);
            r
        })
        .collect();
    buttons.reverse();
    y += row + pad;
    // Centre the whole dialog vertically, a little above the middle.
    let height = y;
    let top = (window.y + (window.height - height) * 0.4).max(window.y).round();
    let shift = |r: Rect| Rect::new(r.x, r.y + top, r.width, r.height);
    DialogLayout {
        panel: Rect::new(x, top, width, height),
        title_y: title_y + top,
        lines: message_lines.into_iter().map(|(l, ly)| (l, ly + top)).collect(),
        input: input.map(shift),
        checkbox: checkbox.map(shift),
        error_y: error_y + top,
        buttons: buttons.into_iter().map(shift).collect(),
    }
}

/// Word-wraps text to `cols` columns, keeping its own line breaks.
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let needed = text_columns(&line) + usize::from(!line.is_empty()) + text_columns(word);
            if !line.is_empty() && needed > cols {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(truncate(&line, cols));
    }
    lines
}

/// What's under a point. `Some(None)` = inside the dialog but on nothing;
/// `None` = outside it.
pub fn hit(layout: &DialogLayout, x: f32, y: f32) -> Option<Option<Target>> {
    if !layout.panel.contains(x, y) {
        return None;
    }
    if let Some(i) = layout.buttons.iter().position(|r| r.contains(x, y)) {
        return Some(Some(Target::Button(i)));
    }
    if layout.checkbox.is_some_and(|r| r.contains(x, y)) {
        return Some(Some(Target::Checkbox));
    }
    if layout.input.is_some_and(|r| r.contains(x, y)) {
        return Some(Some(Target::Input));
    }
    Some(None)
}

/// Where the text field's text starts.
fn text_x(input: Rect, scale: f32) -> f32 {
    input.x + (10.0 * scale).round()
}

pub fn draw<P>(dialog: &Dialog<P>, layout: &DialogLayout, theme: &Theme, m: CellMetrics, scale: f32, out: &mut Vec<ChromeItem>) {
    let bg = theme.background.0;
    let fg = theme.foreground.0;
    let panel_bg = mix(bg, fg, 0.08);
    let muted = mix(fg, panel_bg, 0.4);
    let accent = theme.cursor.0;
    let red = theme.palette().colors[1];
    let edge = scale.round().max(1.0);
    let radius = (10.0 * scale).round();
    let small = (6.0 * scale).round();
    let rounded = |r: Rect, radius: f32, color: Rgb| ChromeItem::RoundedRect {
        rect: [r.x, r.y, r.width, r.height],
        radius,
        top_only: false,
        color,
        alpha: 1.0,
    };
    let text = |text: String, x: f32, y: f32, color: Rgb, bold: bool| ChromeItem::Text { text, x, y, color, bold };
    let center_y = |r: Rect| r.y + ((r.height - m.height) / 2.0).round();
    let p = layout.panel;
    let pad = (18.0 * scale).round();

    out.push(ChromeItem::Rect {
        rect: [0.0, 0.0, 100_000.0, 100_000.0],
        color: Rgb::new(0, 0, 0),
        alpha: 0.35,
    });
    out.push(rounded(
        Rect::new(p.x - edge, p.y - edge, p.width + 2.0 * edge, p.height + 2.0 * edge),
        radius + edge,
        mix(bg, fg, 0.2),
    ));
    out.push(rounded(p, radius, panel_bg));
    out.push(text(dialog.title.clone(), p.x + pad, layout.title_y, fg, true));
    for (line, y) in &layout.lines {
        out.push(text(line.clone(), p.x + pad, *y, mix(fg, panel_bg, 0.15), false));
    }

    if let (Some(r), Some(value)) = (layout.input, &dialog.input) {
        out.push(rounded(
            Rect::new(r.x - edge, r.y - edge, r.width + 2.0 * edge, r.height + 2.0 * edge),
            small + edge,
            accent,
        ));
        out.push(rounded(r, small, mix(bg, fg, 0.03)));
        let cols = ((r.width - (20.0 * scale).round()) / m.width).max(1.0) as usize;
        value.draw(text_x(r, scale), center_y(r), cols, fg, accent, true, m, scale, out);
    }

    if let (Some(r), Some((label, checked))) = (layout.checkbox, &dialog.checkbox) {
        let size = (m.height * 0.8).round();
        let bx = Rect::new(r.x, r.y + ((r.height - size) / 2.0).round(), size, size);
        let hovered = dialog.hover == Some(Target::Checkbox);
        out.push(rounded(bx, (3.0 * scale).round(), if *checked { accent } else { mix(panel_bg, fg, if hovered { 0.35 } else { 0.25 }) }));
        if !checked {
            let inset = edge * 1.5;
            out.push(rounded(
                Rect::new(bx.x + inset, bx.y + inset, bx.width - 2.0 * inset, bx.height - 2.0 * inset),
                (2.0 * scale).round(),
                panel_bg,
            ));
        } else {
            out.push(text("✓".into(), bx.x + ((bx.width - m.width) / 2.0).round(), center_y(bx), readable_on(accent), true));
        }
        out.push(text(format!("{label}  (a)"), bx.x + bx.width + m.width, center_y(r), muted, false));
    }

    if let Some(error) = &dialog.error {
        let cols = ((p.width - 2.0 * pad) / m.width) as usize;
        out.push(text(truncate(error, cols), p.x + pad, layout.error_y, red, false));
    }

    for (i, (button, r)) in dialog.buttons.iter().zip(&layout.buttons).enumerate() {
        let hovered = dialog.hover == Some(Target::Button(i));
        let default = i == dialog.default;
        let base = match (default, button.danger) {
            (true, true) => red,
            (true, false) => accent,
            (false, _) => mix(panel_bg, fg, 0.1),
        };
        let fill = if hovered { mix(base, fg, 0.12) } else { base };
        out.push(rounded(*r, small, fill));
        let label_color = if default { readable_on(fill) } else if button.danger { red } else { fg };
        let label = truncate(&dialog.button_label(button), (r.width / m.width) as usize);
        out.push(text(
            label.clone(),
            r.x + ((r.width - text_columns(&label) as f32 * m.width) / 2.0).round(),
            center_y(*r),
            label_color,
            false,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_pick_buttons() {
        let mut dialog = Dialog::new(
            "Replace?",
            "x exists",
            vec![Button::new("Overwrite", 'o').danger(), Button::new("Rename", 'r'), Button::new("Skip", 's')],
            (),
        )
        .with_checkbox("Do this for all");
        assert_eq!(dialog.key(&Key::Character("a".into()), Some("a"), ModifiersState::empty()), None);
        let answer = dialog.key(&Key::Character("r".into()), Some("r"), ModifiersState::empty()).unwrap();
        assert_eq!((answer.button, answer.checked), (Some(1), true));
        assert_eq!(dialog.key(&Key::Named(NamedKey::Escape), None, ModifiersState::empty()).unwrap().button, None);
    }

    #[test]
    fn input_takes_text() {
        let mut dialog = Dialog::new("New folder", "", vec![Button::new("Create", 'c')], ()).with_input("");
        for c in ["n", "e", "w"] {
            assert!(dialog.key(&Key::Character(c.into()), Some(c), ModifiersState::empty()).is_none());
        }
        dialog.key(&Key::Named(NamedKey::Backspace), None, ModifiersState::empty());
        let answer = dialog.key(&Key::Named(NamedKey::Enter), None, ModifiersState::empty()).unwrap();
        assert_eq!((answer.button, answer.text.as_str()), (Some(0), "ne"));
    }

    #[test]
    fn wraps_words() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("a\nb", 10), ["a", "b"]);
    }
}
