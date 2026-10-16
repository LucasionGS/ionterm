//! The "New connection" / "Edit connection" form, drawn over the terminal.

use ionterm_render::{CellMetrics, ChromeItem, text_columns};
use ionterm_vt::Rgb;
use winit::keyboard::ModifiersState;

use crate::layout::Rect;
use crate::menu::swatch_colors;
use crate::ssh::store::HostForm;
use crate::ssh::{HostExtras, SshHost};
use crate::tabbar::{mix, readable_on, truncate};
use crate::textfield::{Edit, TextField};
use crate::theme::Theme;

const LABELS: [&str; FIELDS] = ["Name", "Address", "User", "Port", "Key file", "Jump host", "Icon", "Group"];
const PLACEHOLDERS: [&str; FIELDS] = [
    "myserver",
    "203.0.113.10 or example.com",
    "your user name",
    "22",
    "~/.ssh/id_ed25519",
    "none",
    "default server icon",
    "none",
];
const FIELDS: usize = 8;
/// Fields that are ssh settings (the rest are ionterm's own).
const SSH_FIELDS: usize = 6;
const PORT: usize = 3;

/// Something in the form that can be focused or clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Field(usize),
    /// A colour swatch; the last one is "no colour".
    Swatch(usize),
    Delete,
    Cancel,
    Save,
    Connect,
}

/// What the form asks the app to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Save { connect: bool },
    Delete,
    Cancel,
}

pub struct Editor {
    pub values: [TextField; FIELDS],
    pub color: Option<Rgb>,
    pub focus: Target,
    /// The alias being edited; `None` for a new connection.
    pub previous_alias: Option<String>,
    /// The host comes from the user's own ssh config: its ssh settings are
    /// shown but not edited here, only ionterm's colour, icon and group.
    pub read_only_ssh: bool,
    pub error: Option<String>,
    /// Delete was clicked once; the next click confirms.
    pub confirm_delete: bool,
    pub hover: Option<Target>,
    swatches: Vec<Rgb>,
}

impl Editor {
    pub fn new(theme: &Theme) -> Self {
        Self {
            values: Default::default(),
            color: None,
            focus: Target::Field(0),
            previous_alias: None,
            read_only_ssh: false,
            error: None,
            confirm_delete: false,
            hover: None,
            swatches: swatch_colors(theme),
        }
    }

    pub fn edit(theme: &Theme, host: &SshHost, extras: Option<&HostExtras>, managed: bool) -> Self {
        let form = HostForm::from_host(host);
        let mut editor = Self::new(theme);
        editor.values = [
            form.alias,
            form.hostname,
            form.user,
            form.port.map(|p| p.to_string()).unwrap_or_default(),
            form.identity_file,
            form.proxy_jump,
            extras.and_then(|e| e.icon.clone()).unwrap_or_default(),
            extras.and_then(|e| e.group.clone()).unwrap_or_default(),
        ]
        .map(TextField::new);
        editor.color = extras.and_then(|e| e.color).map(|c| c.0);
        editor.previous_alias = Some(host.alias.clone());
        editor.read_only_ssh = !managed;
        editor.focus = if managed { Target::Field(0) } else { Target::Field(6) };
        editor
    }

    fn editable(&self, field: usize) -> bool {
        field >= SSH_FIELDS || !self.read_only_ssh
    }

    pub fn can_delete(&self) -> bool {
        self.previous_alias.is_some() && !self.read_only_ssh
    }

    /// The connection settings as entered.
    pub fn form(&self) -> Result<HostForm, String> {
        let v = |i: usize| self.values[i].text().trim().to_string();
        let port = match v(PORT).as_str() {
            "" => None,
            text => Some(
                text.parse::<u16>()
                    .ok()
                    .filter(|&p| p > 0)
                    .ok_or_else(|| format!("\"{text}\" isn't a port number (1–65535)."))?,
            ),
        };
        Ok(HostForm {
            alias: v(0),
            hostname: v(1),
            user: v(2),
            port,
            identity_file: v(4),
            proxy_jump: v(5),
        })
    }

    pub fn icon(&self) -> String {
        self.values[6].text().trim().chars().take(1).collect()
    }

    pub fn group(&self) -> String {
        self.values[7].text().trim().to_string()
    }

    /// Tab order: editable fields, the colour row, then the buttons.
    fn focus_order(&self) -> Vec<Target> {
        let mut order: Vec<Target> = (0..FIELDS).filter(|&f| self.editable(f)).map(Target::Field).collect();
        order.push(Target::Swatch(self.color_index()));
        if self.can_delete() {
            order.push(Target::Delete);
        }
        order.extend([Target::Cancel, Target::Save, Target::Connect]);
        order
    }

    /// The swatch matching the chosen colour (the last one is "none").
    fn color_index(&self) -> usize {
        self.color
            .and_then(|c| self.swatches.iter().position(|&s| s == c))
            .unwrap_or(self.swatches.len())
    }

    fn move_focus(&mut self, delta: isize) {
        let order = self.focus_order();
        let current = order
            .iter()
            .position(|t| match (t, self.focus) {
                (Target::Swatch(_), Target::Swatch(_)) => true,
                (a, b) => *a == b,
            })
            .unwrap_or(0);
        self.focus = order[(current as isize + delta).rem_euclid(order.len() as isize) as usize];
    }

    pub fn type_text(&mut self, text: &str) {
        if let Target::Field(f) = self.focus
            && self.editable(f)
        {
            self.values[f].insert(text);
            self.error = None;
        }
    }

    /// The focused field, if it can be typed in.
    pub fn focused_field(&self) -> Option<&TextField> {
        match self.focus {
            Target::Field(f) if self.editable(f) => Some(&self.values[f]),
            _ => None,
        }
    }

    /// Handles a key; returns what the form wants done, if anything.
    pub fn key(&mut self, key: &winit::keyboard::Key, text: Option<&str>, mods: ModifiersState) -> Option<Outcome> {
        use winit::keyboard::{Key, NamedKey};
        let (ctrl, shift) = (mods.control_key(), mods.shift_key());
        self.confirm_delete = false;
        match key {
            Key::Named(NamedKey::Escape) => return Some(Outcome::Cancel),
            Key::Named(NamedKey::Enter) => {
                return match self.focus {
                    _ if ctrl => Some(Outcome::Save { connect: true }),
                    Target::Cancel => Some(Outcome::Cancel),
                    Target::Connect => Some(Outcome::Save { connect: true }),
                    Target::Delete => {
                        self.confirm_delete = true;
                        None
                    }
                    Target::Field(f) if f + 1 < FIELDS => {
                        self.move_focus(1);
                        None
                    }
                    _ => Some(Outcome::Save { connect: false }),
                };
            }
            Key::Named(NamedKey::Tab) => self.move_focus(if shift { -1 } else { 1 }),
            Key::Named(NamedKey::ArrowDown) => self.move_focus(1),
            Key::Named(NamedKey::ArrowUp) => self.move_focus(-1),
            Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowRight) if matches!(self.focus, Target::Swatch(_)) => {
                let Target::Swatch(i) = self.focus else { return None };
                let last = self.swatches.len();
                let next = if matches!(key, Key::Named(NamedKey::ArrowLeft)) {
                    i.checked_sub(1).unwrap_or(last)
                } else if i >= last {
                    0
                } else {
                    i + 1
                };
                self.pick_swatch(next);
            }
            _ => {
                if let Target::Field(f) = self.focus
                    && self.editable(f)
                    && self.values[f].key(key, text, mods) == Some(Edit::Changed)
                {
                    self.error = None;
                }
            }
        }
        None
    }

    pub fn pick_swatch(&mut self, index: usize) {
        self.color = self.swatches.get(index).copied();
        self.focus = Target::Swatch(index);
    }

    /// A click at `x` in the focused field puts the caret there, or extends
    /// the selection to it.
    pub fn place_caret(&mut self, layout: &EditorLayout, x: f32, m: CellMetrics, scale: f32, extend: bool) {
        if let Target::Field(f) = self.focus
            && self.editable(f)
        {
            let tx = text_x(layout.fields[f].1, scale);
            self.values[f].click((x - tx) / m.width, extend);
        }
    }

    /// Handles a click on a target; returns what the form wants done.
    pub fn click(&mut self, target: Target) -> Option<Outcome> {
        let confirming = self.confirm_delete;
        self.confirm_delete = false;
        match target {
            Target::Field(f) if self.editable(f) => self.focus = target,
            Target::Field(_) => {}
            Target::Swatch(i) => self.pick_swatch(i),
            Target::Cancel => return Some(Outcome::Cancel),
            Target::Save => return Some(Outcome::Save { connect: false }),
            Target::Connect => return Some(Outcome::Save { connect: true }),
            Target::Delete if confirming => return Some(Outcome::Delete),
            Target::Delete => self.confirm_delete = true,
        }
        None
    }
}

pub struct EditorLayout {
    pub panel: Rect,
    pub title_y: f32,
    pub fields: Vec<(Rect, Rect)>,
    pub color_label: Rect,
    pub swatches: Vec<Rect>,
    pub note_y: f32,
    pub buttons: Vec<(Target, Rect, &'static str)>,
}

pub fn layout(editor: &Editor, m: CellMetrics, scale: f32, window: Rect) -> EditorLayout {
    let px = |v: f32| (v * scale).round();
    let pad = px(18.0);
    let width = (m.width * 62.0).min(window.width - px(24.0)).max(m.width * 36.0);
    let row = (m.height * 1.9).round();
    let gap = px(6.0);
    let label_w = m.width * 11.0;
    let title_h = (m.height * 2.0).round();
    let swatch_size = (row * 0.6).round();
    let height = pad + title_h + (FIELDS as f32 + 1.0) * (row + gap) + m.height * 1.6 + row + pad;
    let x = (window.x + (window.width - width) / 2.0).round();
    let y = (window.y + ((window.height - height) / 2.0).max(px(8.0))).round();
    let panel = Rect::new(x, y, width, height);

    let content_x = x + pad;
    let input_x = content_x + label_w;
    let input_w = width - 2.0 * pad - label_w;
    let mut cursor_y = y + pad + title_h;
    let fields = (0..FIELDS)
        .map(|_| {
            let label = Rect::new(content_x, cursor_y, label_w, row);
            let input = Rect::new(input_x, cursor_y, input_w, row);
            cursor_y += row + gap;
            (label, input)
        })
        .collect();
    let color_label = Rect::new(content_x, cursor_y, label_w, row);
    let step = swatch_size + px(8.0);
    let swatches = (0..=editor.swatches.len())
        .map(|i| {
            Rect::new(
                input_x + i as f32 * step,
                cursor_y + ((row - swatch_size) / 2.0).round(),
                swatch_size,
                swatch_size,
            )
        })
        .collect();
    cursor_y += row + gap;
    let note_y = cursor_y + ((m.height * 1.6 - m.height) / 2.0).round();
    cursor_y += m.height * 1.6;

    // Buttons, right-aligned; Delete sits on the left.
    let button = |label: &str| (text_columns(label) as f32 + 4.0) * m.width;
    let mut buttons = Vec::new();
    let mut right = x + width - pad;
    for (target, label) in [(Target::Connect, "Save & connect"), (Target::Save, "Save"), (Target::Cancel, "Cancel")] {
        let w = button(label);
        right -= w;
        buttons.push((target, Rect::new(right, cursor_y, w, row), label));
        right -= px(8.0);
    }
    if editor.can_delete() {
        let label = if editor.confirm_delete { "Really delete?" } else { "Delete" };
        buttons.push((Target::Delete, Rect::new(content_x, cursor_y, button(label), row), label));
    }
    EditorLayout {
        panel,
        title_y: y + pad + ((title_h - m.height) / 2.0).round() - px(4.0),
        fields,
        color_label,
        swatches,
        note_y,
        buttons,
    }
}

/// Where a field's text starts.
fn text_x(input: Rect, scale: f32) -> f32 {
    input.x + (10.0 * scale).round()
}

/// What's under a point. `Some(None)` = inside the form but on nothing;
/// `None` = outside it.
pub fn hit(layout: &EditorLayout, x: f32, y: f32) -> Option<Option<Target>> {
    if !layout.panel.contains(x, y) {
        return None;
    }
    let field = layout.fields.iter().position(|(_, input)| input.contains(x, y)).map(Target::Field);
    let swatch = layout.swatches.iter().position(|r| r.contains(x, y)).map(Target::Swatch);
    let button = layout.buttons.iter().find(|(_, r, _)| r.contains(x, y)).map(|(t, _, _)| *t);
    Some(field.or(swatch).or(button))
}

pub fn draw(editor: &Editor, layout: &EditorLayout, theme: &Theme, m: CellMetrics, scale: f32, out: &mut Vec<ChromeItem>) {
    let bg = theme.background.0;
    let fg = theme.foreground.0;
    let panel_bg = mix(bg, fg, 0.08);
    let muted = mix(fg, panel_bg, 0.45);
    let accent = theme.cursor.0;
    let edge = scale.round().max(1.0);
    let p = layout.panel;
    let radius = (10.0 * scale).round();
    let rounded = |rect: Rect, radius: f32, color: Rgb| ChromeItem::RoundedRect {
        rect: [rect.x, rect.y, rect.width, rect.height],
        radius,
        top_only: false,
        color,
        alpha: 1.0,
    };
    let text = |text: String, x: f32, y: f32, color: Rgb, bold: bool| ChromeItem::Text { text, x, y, color, bold };
    let center_y = |r: Rect| r.y + ((r.height - m.height) / 2.0).round();

    // Dim the terminal behind the form.
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

    let title = match &editor.previous_alias {
        Some(alias) => format!("Edit {alias}"),
        None => "New SSH connection".to_string(),
    };
    out.push(text(title, p.x + (18.0 * scale).round(), layout.title_y, fg, true));

    for (f, (label, input)) in layout.fields.iter().enumerate() {
        let editable = editor.editable(f);
        let focused = editor.focus == Target::Field(f);
        out.push(text(LABELS[f].into(), label.x, center_y(*label), if editable { muted } else { mix(muted, panel_bg, 0.4) }, false));
        if focused {
            out.push(rounded(
                Rect::new(input.x - edge, input.y - edge, input.width + 2.0 * edge, input.height + 2.0 * edge),
                (6.0 * scale).round() + edge,
                accent,
            ));
        }
        out.push(rounded(*input, (6.0 * scale).round(), mix(bg, fg, if editable { 0.03 } else { 0.06 })));
        let value = &editor.values[f];
        let tx = text_x(*input, scale);
        let cols = ((input.width - (20.0 * scale).round()) / m.width).max(1.0) as usize;
        if value.is_empty() {
            out.push(text(truncate(PLACEHOLDERS[f], cols), tx, center_y(*input), mix(muted, panel_bg, 0.3), false));
        }
        if focused || !value.is_empty() {
            let color = if editable { fg } else { muted };
            value.draw(tx, center_y(*input), cols, color, accent, focused, m, scale, out);
        }
    }

    out.push(text("Colour".into(), layout.color_label.x, center_y(layout.color_label), muted, false));
    let chosen = editor.color_index();
    let swatch_focus = matches!(editor.focus, Target::Swatch(_));
    for (i, r) in layout.swatches.iter().enumerate() {
        let selected = i == chosen;
        let hovered = editor.hover == Some(Target::Swatch(i));
        if selected || hovered {
            let ring = (2.0 * scale).round() + if selected && swatch_focus { edge } else { 0.0 };
            out.push(ChromeItem::RoundedRect {
                rect: [r.x - ring, r.y - ring, r.width + 2.0 * ring, r.height + 2.0 * ring],
                radius: r.width / 2.0 + ring,
                top_only: false,
                color: if selected { accent } else { muted },
                alpha: 1.0,
            });
        }
        match editor.swatches.get(i) {
            Some(color) => out.push(ChromeItem::RoundedRect {
                rect: [r.x, r.y, r.width, r.height],
                radius: r.width / 2.0,
                top_only: false,
                color: *color,
                alpha: 1.0,
            }),
            None => {
                out.push(ChromeItem::RoundedRect {
                    rect: [r.x, r.y, r.width, r.height],
                    radius: r.width / 2.0,
                    top_only: false,
                    color: panel_bg,
                    alpha: 1.0,
                });
                out.push(text(
                    "╱".into(),
                    r.x + ((r.width - m.width) / 2.0).round(),
                    r.y + ((r.height - m.height) / 2.0).round(),
                    muted,
                    false,
                ));
            }
        }
    }

    // Error, or a note about where the settings come from.
    let note = match (&editor.error, editor.read_only_ssh) {
        (Some(error), _) => Some((error.clone(), theme.palette().colors[1])),
        (None, true) => Some(("Defined in your own ~/.ssh/config: edit it there to change these.".to_string(), muted)),
        (None, false) => None,
    };
    if let Some((message, color)) = note {
        let cols = ((p.width - (36.0 * scale).round()) / m.width) as usize;
        out.push(text(truncate(&message, cols), p.x + (18.0 * scale).round(), layout.note_y, color, false));
    }

    for &(target, r, label) in &layout.buttons {
        let hovered = editor.hover == Some(target);
        let focused = editor.focus == target;
        let primary = target == Target::Connect;
        let danger = target == Target::Delete;
        let base = if primary {
            accent
        } else if danger && editor.confirm_delete {
            theme.palette().colors[1]
        } else {
            mix(panel_bg, fg, 0.1)
        };
        let fill = if hovered { mix(base, fg, 0.12) } else { base };
        if focused {
            out.push(rounded(
                Rect::new(r.x - 2.0 * edge, r.y - 2.0 * edge, r.width + 4.0 * edge, r.height + 4.0 * edge),
                (6.0 * scale).round() + 2.0 * edge,
                fg,
            ));
        }
        out.push(rounded(r, (6.0 * scale).round(), fill));
        let label_color = if primary || (danger && editor.confirm_delete) { readable_on(fill) } else { fg };
        out.push(text(
            label.into(),
            r.x + ((r.width - text_columns(label) as f32 * m.width) / 2.0).round(),
            center_y(r),
            label_color,
            false,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_order_and_saving() {
        let theme = Theme::default();
        let mut editor = Editor::new(&theme);
        for c in "box".chars() {
            editor.key(&winit::keyboard::Key::Character(c.to_string().into()), Some(&c.to_string()), ModifiersState::empty());
        }
        editor.key(&winit::keyboard::Key::Named(winit::keyboard::NamedKey::Tab), None, ModifiersState::empty());
        editor.type_text("10.0.0.2\n");
        assert_eq!(editor.values[1].text(), "10.0.0.2");
        editor.focus = Target::Field(PORT);
        editor.type_text("abc");
        assert!(editor.form().is_err());
        editor.values[PORT].set("2200");
        assert_eq!(editor.form().unwrap().port, Some(2200));

        // Hosts from the user's config only expose ionterm's own fields.
        let host = SshHost {
            alias: "theirs".into(),
            ..SshHost::default()
        };
        let editor = Editor::edit(&theme, &host, None, false);
        assert_eq!(editor.focus, Target::Field(6));
        assert!(!editor.can_delete());
        assert!(!editor.focus_order().contains(&Target::Field(0)));
    }
}
