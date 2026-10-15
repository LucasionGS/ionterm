//! The settings page, shown in its own tab: simple settings as controls
//! (switches, choices, steppers, text, a font picker) that write straight
//! to `ionterm.toml`, and buttons that open the config files in `$EDITOR`.
//!
//! Like the other widgets: `layout()` is pure geometry, `hit()` finds what's
//! under the mouse, `draw()` builds `ChromeItem`s, and keys and clicks
//! return a `Request` for the app to carry out.

use ionterm_render::{CellMetrics, ChromeItem, text_columns};
use ionterm_vt::Rgb;
use winit::keyboard::{Key, NamedKey};

use crate::config::{Config, SettingValue};
use crate::layout::Rect;
use crate::tabbar::{mix, readable_on, truncate};
use crate::theme::Theme;

//#region The settings

/// A config file the page opens in the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Config,
    Theme,
    SshConfig,
    SshIonterm,
}

#[derive(Clone, Copy, Debug)]
pub enum Control {
    Toggle,
    /// (value in the file, label).
    Choice(&'static [(&'static str, &'static str)]),
    Number { min: f64, max: f64, step: f64, decimals: usize, unit: &'static str },
    /// A number that may be left unset (to follow something else).
    Optional { min: f64, max: f64, step: f64, unset: &'static str },
    Text { placeholder: &'static str },
    Font,
    Edit(FileKind),
}

pub struct Setting {
    pub section: &'static str,
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub control: Control,
}

pub struct Section {
    pub title: &'static str,
    pub icon: char,
    pub settings: &'static [Setting],
}

const fn setting(section: &'static str, key: &'static str, label: &'static str, help: &'static str, control: Control) -> Setting {
    Setting {
        section,
        key,
        label,
        help,
        control,
    }
}

const CURSOR_SHAPES: &[(&str, &str)] = &[("beam", "Beam"), ("block", "Block"), ("underline", "Underline")];
const DECORATIONS: &[(&str, &str)] = &[("auto", "Auto"), ("full", "Full"), ("none", "None")];
const TAB_STYLES: &[(&str, &str)] = &[("tabs", "Tabs"), ("slanted", "Slanted"), ("angled", "Angled"), ("round", "Round")];
const TAB_EDGES: &[(&str, &str)] = &[("top", "Top"), ("bottom", "Bottom")];
const TAB_SHOW: &[(&str, &str)] = &[("auto", "2+ tabs"), ("always", "Always"), ("never", "Never")];

pub const SECTIONS: &[Section] = &[
    Section {
        title: "Appearance",
        icon: '\u{F03D8}',
        settings: &[
            setting("font", "family", "Font", "Monospace fonts installed on this computer.", Control::Font),
            setting("font", "size", "Font size", "In points.", Control::Number { min: 6.0, max: 40.0, step: 0.5, decimals: 1, unit: "pt" }),
            setting("font", "ligatures", "Ligatures", "Join -> != === and friends into one glyph.", Control::Toggle),
            setting("cursor", "shape", "Cursor shape", "Programs can still ask for their own.", Control::Choice(CURSOR_SHAPES)),
            setting("cursor", "blink_interval", "Cursor blink", "Seconds per blink; 0 keeps it steady.", Control::Number { min: 0.0, max: 2.0, step: 0.1, decimals: 1, unit: "s" }),
            setting("window", "opacity", "Background opacity", "Unset follows the theme.", Control::Optional { min: 0.2, max: 1.0, step: 0.05, unset: "Theme" }),
            setting("window", "background_image", "Background image", "A PNG, JPEG or WebP behind the text; empty for none.", Control::Text { placeholder: "~/Pictures/wallpaper.jpg" }),
            setting("window", "background_image_opacity", "Image strength", "How strongly the picture shows through.", Control::Number { min: 0.05, max: 1.0, step: 0.05, decimals: 2, unit: "" }),
            setting("window", "padding", "Padding", "Space around the text.", Control::Number { min: 0.0, max: 40.0, step: 1.0, decimals: 0, unit: "px" }),
            setting("window", "decorations", "Window decorations", "Auto: none under Hyprland, which draws its own borders.", Control::Choice(DECORATIONS)),
        ],
    },
    Section {
        title: "Tab bar",
        icon: '\u{F04E9}',
        settings: &[
            setting("tab_bar", "style", "Style", "Windows Terminal style tabs, or kitty's Powerline strip.", Control::Choice(TAB_STYLES)),
            setting("tab_bar", "edge", "Position", "Where the bar sits.", Control::Choice(TAB_EDGES)),
            setting("tab_bar", "show", "Show", "When the bar is visible.", Control::Choice(TAB_SHOW)),
            setting("tab_bar", "max_title_length", "Title length", "Longest tab title in the Powerline styles.", Control::Number { min: 8.0, max: 80.0, step: 1.0, decimals: 0, unit: "" }),
        ],
    },
    Section {
        title: "Terminal",
        icon: '\u{F120}',
        settings: &[
            setting("terminal", "shell", "Shell", "Command line to run; empty runs your login shell.", Control::Text { placeholder: "your login shell" }),
            setting("terminal", "shell_integration", "Shell integration", "Prompt jumps, output selection and failed marks in bash, zsh and fish.", Control::Toggle),
            setting("terminal", "scrollback_lines", "Scrollback", "Lines kept above the screen.", Control::Number { min: 0.0, max: 200_000.0, step: 1000.0, decimals: 0, unit: "lines" }),
            setting("terminal", "wheel_scroll_lines", "Wheel scroll", "Lines per mouse wheel notch.", Control::Number { min: 1.0, max: 20.0, step: 1.0, decimals: 0, unit: "lines" }),
            setting("terminal", "scrollback_pager", "Scrollback pager", "Shows the scrollback (ctrl+shift+h); INPUT_LINE_NUMBER is the line on screen.", Control::Text { placeholder: "less -R +INPUT_LINE_NUMBER" }),
            setting("terminal", "confirm_multiline_paste", "Confirm multi-line paste", "Ask first when pasted lines would run one by one (programs without bracketed paste).", Control::Toggle),
            setting("session", "restore", "Restore session", "Reopen the last window's tabs, splits, folders and SSH connections.", Control::Toggle),
            setting("terminal", "remote_control", "Remote control", "Let scripts drive ionterm with \"ionterm @\" (takes effect in new windows).", Control::Toggle),
            setting("terminal", "advertise_sixel", "Announce Sixel", "Sixel images show either way; announcing it makes yazi prefer it over kitty graphics.", Control::Toggle),
            setting("layouts", "border_width", "Split borders", "Width of the line between panes.", Control::Number { min: 0.0, max: 10.0, step: 1.0, decimals: 0, unit: "px" }),
            setting("notifications", "command_done_after", "Notify after", "Announce commands that ran this long when you're in another tab or window; 0 turns it off.", Control::Number { min: 0.0, max: 600.0, step: 5.0, decimals: 0, unit: "s" }),
            setting("notifications", "from_programs", "Program notifications", "Let programs pop up desktop notifications (OSC 9 and 777).", Control::Toggle),
        ],
    },
    Section {
        title: "Files",
        icon: '\u{F024B}',
        settings: &[
            setting("files", "download_dir", "Downloads", "Where downloads go; empty uses your Downloads folder.", Control::Text { placeholder: "~/Downloads" }),
            setting("files", "show_hidden", "Show hidden files", "Dotfiles in the SSH file panel (ctrl+h toggles).", Control::Toggle),
            setting("files", "panel_width", "File panel width", "In columns.", Control::Number { min: 24.0, max: 120.0, step: 2.0, decimals: 0, unit: "cols" }),
            setting("hints", "open_file", "Open files with", "For hints (ctrl+shift+e): {path} {line} {col}; empty uses $EDITOR in a new tab.", Control::Text { placeholder: "$EDITOR in a new tab" }),
        ],
    },
    Section {
        title: "Config files",
        icon: '\u{F0219}',
        settings: &[
            setting("", "", "ionterm.toml", "Every setting, keybindings included.", Control::Edit(FileKind::Config)),
            setting("", "", "theme.toml", "Colours, when the Ionix styler doesn't manage them.", Control::Edit(FileKind::Theme)),
            setting("", "", "SSH config", "~/.ssh/config: your own hosts.", Control::Edit(FileKind::SshConfig)),
            setting("", "", "ionterm's SSH hosts", "~/.ssh/config.d/ionterm: hosts added in ionterm.", Control::Edit(FileKind::SshIonterm)),
        ],
    },
];

/// A setting's value as shown.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Number(f64),
    Text(String),
    Unset,
}

/// The value a setting has in `config`.
pub fn value_of(config: &Config, setting: &Setting) -> Value {
    use crate::config::{CursorShapeConfig as Cs, Decorations as D, ShowTabBar as Sh, TabBarEdge as E, TabBarStyleConfig as St};
    let text = |s: &str| Value::Text(s.to_string());
    match (setting.section, setting.key) {
        ("font", "family") => Value::Text(config.font.family.clone()),
        ("font", "size") => Value::Number(f64::from(config.font.size)),
        ("font", "ligatures") => Value::Bool(config.font.ligatures),
        ("cursor", "shape") => text(match config.cursor.shape {
            Cs::Block => "block",
            Cs::Beam => "beam",
            Cs::Underline => "underline",
        }),
        ("cursor", "blink_interval") => Value::Number(f64::from(config.cursor.blink_interval)),
        ("window", "opacity") => config.window.opacity.map_or(Value::Unset, |o| Value::Number(f64::from(o))),
        ("window", "padding") => Value::Number(f64::from(config.window.padding)),
        ("window", "background_image") => Value::Text(config.window.background_image.clone()),
        ("window", "background_image_opacity") => Value::Number(f64::from(config.window.background_image_opacity)),
        ("window", "decorations") => text(match config.window.decorations {
            D::Auto => "auto",
            D::Full => "full",
            D::None => "none",
        }),
        ("tab_bar", "style") => text(match config.tab_bar.style {
            St::Tabs => "tabs",
            St::Slanted => "slanted",
            St::Angled => "angled",
            St::Round => "round",
        }),
        ("tab_bar", "edge") => text(match config.tab_bar.edge {
            E::Top => "top",
            E::Bottom => "bottom",
        }),
        ("tab_bar", "show") => text(match config.tab_bar.show {
            Sh::Auto => "auto",
            Sh::Always => "always",
            Sh::Never => "never",
        }),
        ("tab_bar", "max_title_length") => Value::Number(config.tab_bar.max_title_length as f64),
        ("terminal", "shell") => Value::Text(config.terminal.shell.clone()),
        ("terminal", "shell_integration") => Value::Bool(config.terminal.shell_integration),
        ("terminal", "scrollback_lines") => Value::Number(config.terminal.scrollback_lines as f64),
        ("terminal", "wheel_scroll_lines") => Value::Number(config.terminal.wheel_scroll_lines as f64),
        ("terminal", "advertise_sixel") => Value::Bool(config.terminal.advertise_sixel),
        ("terminal", "confirm_multiline_paste") => Value::Bool(config.terminal.confirm_multiline_paste),
        ("terminal", "scrollback_pager") => Value::Text(config.terminal.scrollback_pager.clone()),
        ("terminal", "remote_control") => Value::Bool(config.terminal.remote_control),
        ("session", "restore") => Value::Bool(config.session.restore),
        ("layouts", "border_width") => Value::Number(f64::from(config.layouts.border_width)),
        ("notifications", "command_done_after") => Value::Number(f64::from(config.notifications.command_done_after)),
        ("notifications", "from_programs") => Value::Bool(config.notifications.from_programs),
        ("files", "download_dir") => Value::Text(config.files.download_dir.clone()),
        ("files", "show_hidden") => Value::Bool(config.files.show_hidden),
        ("files", "panel_width") => Value::Number(f64::from(config.files.panel_width)),
        ("hints", "open_file") => Value::Text(config.hints.open_file.clone()),
        _ => Value::Unset,
    }
}

/// How a value is written to the file: integers where the config wants
/// them.
fn to_setting(setting: &Setting, value: Value) -> Option<SettingValue> {
    let integer = matches!((setting.section, setting.key), ("tab_bar", "max_title_length") | ("terminal", "scrollback_lines" | "wheel_scroll_lines") | ("files", "panel_width"));
    match value {
        Value::Bool(b) => Some(SettingValue::Bool(b)),
        Value::Number(n) if integer => Some(SettingValue::Int(n.round() as i64)),
        Value::Number(n) => Some(SettingValue::Float(n)),
        Value::Text(t) => Some(SettingValue::Text(t)),
        Value::Unset => None,
    }
}

//#endregion

//#region State and input

/// What the page asks the app to do.
#[derive(Debug, PartialEq)]
pub enum Request {
    /// Write `section.key` (`None` = remove it, back to the default).
    Set {
        section: &'static str,
        key: &'static str,
        value: Option<SettingValue>,
    },
    Edit(FileKind),
    /// Open the font picker (the app knows which fonts are installed).
    PickFont,
}

/// Something on the page that can be clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Nav(usize),
    Row(usize),
    Toggle(usize),
    Choice(usize, usize),
    Minus(usize),
    Plus(usize),
    Text(usize),
    Button(usize),
    Reset(usize),
    PickerRow(usize),
}

pub struct FontPicker {
    pub families: Vec<String>,
    pub filter: String,
    pub selected: usize,
    pub scroll: usize,
}

impl FontPicker {
    pub fn new(mut families: Vec<String>, current: &str) -> Self {
        // The configured font is always there to go back to.
        if !current.is_empty() && !families.iter().any(|f| f == current) {
            families.push(current.to_string());
            families.sort_by_key(|f| f.to_lowercase());
        }
        let selected = families.iter().position(|f| f == current).unwrap_or(0);
        Self {
            families,
            filter: String::new(),
            selected,
            scroll: selected.saturating_sub(4),
        }
    }

    pub fn shown(&self) -> Vec<&String> {
        let filter = self.filter.to_lowercase();
        self.families.iter().filter(|f| f.to_lowercase().contains(&filter)).collect()
    }
}

pub struct SettingsPage {
    pub section: usize,
    /// The focused row in the section.
    pub focus: usize,
    /// Rows scrolled off the top.
    pub scroll: usize,
    /// The text being typed into a text setting.
    pub typing: Option<String>,
    pub picker: Option<FontPicker>,
    pub hover: Option<Target>,
    /// A config file is open in the editor in this tab.
    pub editing: bool,
    /// Rows that fit on screen, from the last layout.
    pub visible: usize,
}

impl Default for SettingsPage {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsPage {
    pub fn new() -> Self {
        Self {
            section: 0,
            focus: 0,
            scroll: 0,
            typing: None,
            picker: None,
            hover: None,
            editing: false,
            visible: 6,
        }
    }

    /// Scrolls so the focused row is on screen.
    fn keep_focus_visible(&mut self) {
        if self.focus < self.scroll {
            self.scroll = self.focus;
        } else if self.focus >= self.scroll + self.visible {
            self.scroll = self.focus + 1 - self.visible;
        }
    }

    fn settings(&self) -> &'static [Setting] {
        SECTIONS[self.section].settings
    }

    fn select_section(&mut self, section: usize) {
        self.section = section.min(SECTIONS.len() - 1);
        self.focus = 0;
        self.scroll = 0;
        self.typing = None;
    }

    /// A new value for a setting, or `None` when it's the default (so the
    /// key comes out of the file rather than pinning today's default).
    fn set(setting: &'static Setting, value: Value, defaults: &Config) -> Request {
        let value = if value == value_of(defaults, setting) { None } else { to_setting(setting, value) };
        Request::Set {
            section: setting.section,
            key: setting.key,
            value,
        }
    }

    /// Changes a setting by one step: `delta` is -1 or +1.
    fn step(&self, row: usize, delta: i32, config: &Config) -> Option<Request> {
        let setting = self.settings().get(row)?;
        let defaults = Config::default();
        let current = value_of(config, setting);
        let value = match (setting.control, current) {
            (Control::Toggle, Value::Bool(_)) => Value::Bool(delta > 0),
            (Control::Choice(options), Value::Text(now)) => {
                let i = options.iter().position(|(v, _)| *v == now).unwrap_or(0) as i32;
                let next = (i + delta).rem_euclid(options.len() as i32) as usize;
                Value::Text(options[next].0.to_string())
            }
            (Control::Number { min, max, step, .. }, Value::Number(now)) => {
                Value::Number(((now / step).round() + f64::from(delta)).mul_add(step, 0.0).clamp(min, max))
            }
            (Control::Optional { min, max, step, .. }, now) => match now {
                Value::Number(n) if delta > 0 && n >= max => Value::Unset,
                Value::Number(n) => Value::Number((n + f64::from(delta) * step).clamp(min, max)),
                // From "theme" down to the top of the range.
                _ if delta < 0 => Value::Number(max),
                _ => return None,
            },
            _ => return None,
        };
        Some(Self::set(setting, value, &defaults))
    }

    /// Enter or Space on a row: flip, open, start typing.
    fn activate(&mut self, row: usize, config: &Config) -> Option<Request> {
        let setting = self.settings().get(row)?;
        match setting.control {
            Control::Toggle => {
                let on = matches!(value_of(config, setting), Value::Bool(true));
                self.step(row, if on { -1 } else { 1 }, config)
            }
            Control::Choice(_) => self.step(row, 1, config),
            Control::Text { .. } => {
                if let Value::Text(text) = value_of(config, setting) {
                    self.typing = Some(text);
                }
                None
            }
            Control::Font => Some(Request::PickFont),
            Control::Edit(kind) => Some(Request::Edit(kind)),
            Control::Number { .. } | Control::Optional { .. } => None,
        }
    }

    /// A key press. Returns what to do, if anything.
    pub fn key(&mut self, key: &Key, text: Option<&str>, config: &Config) -> Option<Request> {
        if let Some(picker) = &mut self.picker {
            return picker_key(picker, key, text).map(|picked| {
                self.picker = None;
                let setting = SECTIONS[0].settings.iter().find(|s| matches!(s.control, Control::Font)).expect("a font row");
                picked.map(|family| Self::set(setting, Value::Text(family), &Config::default()))
            })?;
        }
        if let Some(typing) = &mut self.typing {
            match key {
                Key::Named(NamedKey::Enter) => {
                    let text = std::mem::take(typing).trim().to_string();
                    self.typing = None;
                    let setting = self.settings().get(self.focus)?;
                    return Some(Self::set(setting, Value::Text(text), &Config::default()));
                }
                Key::Named(NamedKey::Escape) => self.typing = None,
                Key::Named(NamedKey::Backspace) => {
                    typing.pop();
                }
                _ => {
                    if let Some(text) = text.filter(|t| !t.chars().any(char::is_control)) {
                        typing.push_str(text);
                    }
                }
            }
            return None;
        }
        let rows = self.settings().len();
        match key {
            Key::Named(NamedKey::ArrowUp) => {
                self.focus = (self.focus + rows - 1) % rows;
                self.keep_focus_visible();
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.focus = (self.focus + 1) % rows;
                self.keep_focus_visible();
            }
            Key::Named(NamedKey::Tab) => self.select_section((self.section + 1) % SECTIONS.len()),
            Key::Named(NamedKey::PageUp) => self.select_section(self.section.saturating_sub(1)),
            Key::Named(NamedKey::PageDown) => self.select_section(self.section + 1),
            Key::Named(NamedKey::ArrowLeft) => return self.step(self.focus, -1, config),
            Key::Named(NamedKey::ArrowRight) => return self.step(self.focus, 1, config),
            Key::Named(NamedKey::Enter | NamedKey::Space) => return self.activate(self.focus, config),
            Key::Named(NamedKey::Delete) => return self.reset(self.focus),
            Key::Character(c) if c == " " => return self.activate(self.focus, config),
            _ => {}
        }
        None
    }

    /// Back to the default.
    fn reset(&self, row: usize) -> Option<Request> {
        let setting = self.settings().get(row)?;
        if matches!(setting.control, Control::Edit(_)) {
            return None;
        }
        Some(Request::Set {
            section: setting.section,
            key: setting.key,
            value: None,
        })
    }

    /// A click on the page.
    pub fn click(&mut self, target: Option<Target>, config: &Config) -> Option<Request> {
        if self.picker.is_some() {
            match target {
                Some(Target::PickerRow(i)) => {
                    let picker = self.picker.take()?;
                    let family = picker.shown().get(i)?.to_string();
                    let setting = SECTIONS[0].settings.iter().find(|s| matches!(s.control, Control::Font))?;
                    return Some(Self::set(setting, Value::Text(family), &Config::default()));
                }
                // A click outside the list closes it.
                _ => self.picker = None,
            }
            return None;
        }
        let target = target?;
        if !matches!(target, Target::Text(_)) {
            self.typing = None;
        }
        match target {
            Target::Nav(section) => self.select_section(section),
            Target::Row(row) => self.focus = row,
            Target::Toggle(row) | Target::Button(row) => {
                self.focus = row;
                return self.activate(row, config);
            }
            Target::Choice(row, option) => {
                self.focus = row;
                let setting = self.settings().get(row)?;
                let Control::Choice(options) = setting.control else { return None };
                return Some(Self::set(setting, Value::Text(options.get(option)?.0.to_string()), &Config::default()));
            }
            Target::Minus(row) => {
                self.focus = row;
                return self.step(row, -1, config);
            }
            Target::Plus(row) => {
                self.focus = row;
                return self.step(row, 1, config);
            }
            Target::Text(row) => {
                if self.focus != row || self.typing.is_none() {
                    self.focus = row;
                    return self.activate(row, config);
                }
            }
            Target::Reset(row) => return self.reset(row),
            Target::PickerRow(_) => {}
        }
        None
    }

    /// The mouse wheel over the page (or the font list).
    pub fn scroll_by(&mut self, lines: isize) {
        if let Some(picker) = &mut self.picker {
            let max = picker.shown().len().saturating_sub(1);
            picker.scroll = (picker.scroll as isize + lines).clamp(0, max as isize) as usize;
        } else {
            let max = self.settings().len().saturating_sub(1);
            self.scroll = (self.scroll as isize + lines).clamp(0, max as isize) as usize;
        }
    }
}

/// Keys in the font picker: `Some(Some(family))` picks, `Some(None)` closes.
fn picker_key(picker: &mut FontPicker, key: &Key, text: Option<&str>) -> Option<Option<String>> {
    let count = picker.shown().len();
    match key {
        Key::Named(NamedKey::Escape) => return Some(None),
        Key::Named(NamedKey::Enter) => return Some(picker.shown().get(picker.selected).map(|f| f.to_string())),
        Key::Named(NamedKey::ArrowUp) => picker.selected = picker.selected.saturating_sub(1),
        Key::Named(NamedKey::ArrowDown) => picker.selected = (picker.selected + 1).min(count.saturating_sub(1)),
        Key::Named(NamedKey::PageUp) => picker.selected = picker.selected.saturating_sub(10),
        Key::Named(NamedKey::PageDown) => picker.selected = (picker.selected + 10).min(count.saturating_sub(1)),
        Key::Named(NamedKey::Backspace) => {
            picker.filter.pop();
            picker.selected = 0;
        }
        _ => {
            if let Some(text) = text.filter(|t| !t.chars().any(char::is_control)) {
                picker.filter.push_str(text);
                picker.selected = 0;
            }
        }
    }
    None
}

//#endregion

//#region Layout and drawing

/// Where a row's pieces are.
pub struct RowLayout {
    pub index: usize,
    pub rect: Rect,
    pub label_y: f32,
    pub help_y: f32,
    pub text_x: f32,
    /// The control's clickable parts.
    pub parts: Vec<(Target, Rect)>,
    pub reset: Rect,
}

pub struct PageLayout {
    pub area: Rect,
    pub nav: Vec<(usize, Rect)>,
    pub title: (f32, f32),
    pub rows: Vec<RowLayout>,
    /// Rows that fit; the page scrolls by rows.
    pub visible: usize,
    pub picker: Option<PickerLayout>,
}

pub struct PickerLayout {
    pub panel: Rect,
    pub search: Rect,
    pub rows: Vec<(usize, Rect)>,
}

pub fn layout(page: &SettingsPage, area: Rect, m: CellMetrics, scale: f32) -> PageLayout {
    let px = |v: f32| (v * scale).round();
    let pad = px(24.0);
    let nav_width = m.width * 22.0;
    let nav_row = (m.height * 2.0).round();
    let nav = (0..SECTIONS.len())
        .map(|i| (i, Rect::new(area.x + pad, area.y + pad + m.height * 2.5 + i as f32 * nav_row, nav_width, nav_row)))
        .collect();

    let content_x = area.x + pad + nav_width + px(32.0);
    let content_width = (area.x + area.width - pad - content_x).min(m.width * 100.0).max(m.width * 30.0);
    let top = area.y + pad + m.height * 2.5;
    let row_height = (m.height * 3.2).round();
    let visible = (((area.y + area.height - pad - top) / row_height).floor() as usize).max(1);

    let settings = SECTIONS[page.section].settings;
    let rows = settings
        .iter()
        .enumerate()
        .skip(page.scroll)
        .take(visible)
        .enumerate()
        .map(|(slot, (index, setting))| {
            let rect = Rect::new(content_x, top + slot as f32 * row_height, content_width, row_height - px(6.0));
            let inner_pad = px(12.0);
            let label_y = rect.y + (rect.height / 2.0 - m.height).round();
            let control_h = (m.height * 1.5).round();
            let control_y = rect.y + ((rect.height - control_h) / 2.0).round();
            let right = rect.x + rect.width - inner_pad;
            let parts = control_parts(setting, index, right, control_y, control_h, m, scale);
            let left = parts.iter().map(|(_, r)| r.x).fold(right, f32::min);
            let reset = Rect::new(left - px(8.0) - control_h, control_y, control_h, control_h);
            RowLayout {
                index,
                rect,
                label_y,
                help_y: label_y + m.height,
                text_x: rect.x + inner_pad,
                parts,
                reset,
            }
        })
        .collect();

    let picker = page.picker.as_ref().map(|picker| {
        let width = (m.width * 48.0).min(area.width - 2.0 * pad);
        let list_rows = 12;
        let row = (m.height * 1.6).round();
        let height = row * (list_rows as f32 + 1.5) + px(24.0);
        let panel = Rect::new(
            area.x + ((area.width - width) / 2.0).round(),
            area.y + ((area.height - height) / 2.0).round().max(pad),
            width,
            height,
        );
        let search = Rect::new(panel.x + px(12.0), panel.y + px(12.0), width - px(24.0), row);
        let rows = (picker.scroll..picker.shown().len())
            .take(list_rows)
            .enumerate()
            .map(|(slot, i)| (i, Rect::new(search.x, search.y + row * 1.25 + slot as f32 * row, search.width, row)))
            .collect();
        PickerLayout { panel, search, rows }
    });

    PageLayout {
        area,
        nav,
        title: (area.x + pad, area.y + pad),
        rows,
        visible,
        picker,
    }
}

/// The clickable parts of a row's control, right-aligned at `right`.
fn control_parts(setting: &Setting, index: usize, right: f32, y: f32, h: f32, m: CellMetrics, scale: f32) -> Vec<(Target, Rect)> {
    let px = |v: f32| (v * scale).round();
    match setting.control {
        Control::Toggle => vec![(Target::Toggle(index), Rect::new(right - h * 1.9, y, h * 1.9, h))],
        Control::Choice(options) => {
            let widths: Vec<f32> = options.iter().map(|(_, label)| (text_columns(label) as f32 + 2.0) * m.width).collect();
            let mut x = right - widths.iter().sum::<f32>();
            options
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let rect = Rect::new(x, y, widths[i], h);
                    x += widths[i];
                    (Target::Choice(index, i), rect)
                })
                .collect()
        }
        Control::Number { .. } | Control::Optional { .. } => {
            let button = h;
            let value = m.width * 10.0;
            let plus = Rect::new(right - button, y, button, h);
            let field = Rect::new(plus.x - px(4.0) - value, y, value, h);
            let minus = Rect::new(field.x - px(4.0) - button, y, button, h);
            vec![(Target::Minus(index), minus), (Target::Row(index), field), (Target::Plus(index), plus)]
        }
        Control::Text { .. } => vec![(Target::Text(index), Rect::new(right - m.width * 34.0, y, m.width * 34.0, h))],
        Control::Font => vec![(Target::Button(index), Rect::new(right - m.width * 34.0, y, m.width * 34.0, h))],
        Control::Edit(_) => vec![(Target::Button(index), Rect::new(right - m.width * 18.0, y, m.width * 18.0, h))],
    }
}

pub fn hit(layout: &PageLayout, x: f32, y: f32) -> Option<Target> {
    if let Some(picker) = &layout.picker {
        return picker.rows.iter().find(|(_, r)| r.contains(x, y)).map(|(i, _)| Target::PickerRow(*i));
    }
    if let Some((i, _)) = layout.nav.iter().find(|(_, r)| r.contains(x, y)) {
        return Some(Target::Nav(*i));
    }
    for row in &layout.rows {
        if let Some((target, _)) = row.parts.iter().find(|(_, r)| r.contains(x, y)) {
            return Some(*target);
        }
        if row.reset.contains(x, y) {
            return Some(Target::Reset(row.index));
        }
        if row.rect.contains(x, y) {
            return Some(Target::Row(row.index));
        }
    }
    None
}

pub struct DrawContext<'a> {
    pub config: &'a Config,
    /// The editor shown on the edit buttons ("nvim").
    pub editor_name: &'a str,
    /// The terminal's background opacity, so the page looks like the tabs.
    pub opacity: f32,
}

pub fn draw(page: &SettingsPage, layout: &PageLayout, ctx: &DrawContext, theme: &Theme, m: CellMetrics, scale: f32, out: &mut Vec<ChromeItem>) {
    let px = |v: f32| (v * scale).round();
    let bg = theme.background.0;
    let fg = theme.foreground.0;
    let muted = mix(fg, bg, 0.45);
    let faint = mix(fg, bg, 0.7);
    let accent = theme.cursor.0;
    let card = mix(bg, fg, 0.05);
    let control_bg = mix(bg, fg, 0.1);
    let text = |text: String, x: f32, y: f32, color: Rgb, bold: bool| ChromeItem::Text { text, x, y, color, bold };
    let rounded = |r: Rect, radius: f32, color: Rgb| ChromeItem::RoundedRect {
        rect: [r.x, r.y, r.width, r.height],
        radius,
        top_only: false,
        color,
        alpha: 1.0,
    };
    let center = |r: Rect, cols: usize| (r.x + ((r.width - cols as f32 * m.width) / 2.0).round(), r.y + ((r.height - m.height) / 2.0).round());

    let a = layout.area;
    out.push(ChromeItem::Rect {
        rect: [a.x, a.y, a.width, a.height],
        color: bg,
        alpha: ctx.opacity,
    });
    out.push(text("\u{F013}  Settings".into(), layout.title.0, layout.title.1, fg, true));

    for (i, r) in &layout.nav {
        let selected = *i == page.section;
        let hovered = page.hover == Some(Target::Nav(*i));
        if selected || hovered {
            out.push(rounded(*r, px(6.0), if selected { mix(bg, accent, 0.18) } else { card }));
        }
        if selected {
            out.push(rounded(Rect::new(r.x, r.y + px(6.0), px(3.0), r.height - px(12.0)), px(2.0), accent));
        }
        let section = &SECTIONS[*i];
        let y = r.y + ((r.height - m.height) / 2.0).round();
        out.push(text(section.icon.to_string(), r.x + px(12.0), y, if selected { accent } else { muted }, false));
        out.push(text(section.title.into(), r.x + px(12.0) + 2.5 * m.width, y, if selected { fg } else { muted }, selected));
    }

    let settings = SECTIONS[page.section].settings;
    let defaults = Config::default();
    for row in &layout.rows {
        let setting = &settings[row.index];
        let focused = row.index == page.focus;
        if focused || page.hover.is_some_and(|t| target_row(t) == Some(row.index)) {
            out.push(rounded(row.rect, px(8.0), card));
        }
        if focused {
            out.push(rounded(Rect::new(row.rect.x, row.rect.y + px(10.0), px(3.0), row.rect.height - px(20.0)), px(2.0), accent));
        }
        let first_part = row.parts.iter().map(|(_, r)| r.x).fold(f32::MAX, f32::min);
        let text_cols = ((first_part - row.text_x - row.reset.width - px(16.0)) / m.width).max(4.0) as usize;
        out.push(text(truncate(setting.label, text_cols), row.text_x, row.label_y, fg, false));
        out.push(text(truncate(setting.help, text_cols), row.text_x, row.help_y, muted, false));

        let value = value_of(ctx.config, setting);
        let changed = !matches!(setting.control, Control::Edit(_)) && value != value_of(&defaults, setting);
        if changed {
            let hovered = page.hover == Some(Target::Reset(row.index));
            if hovered {
                out.push(rounded(row.reset, px(6.0), control_bg));
            }
            let (x, y) = center(row.reset, 1);
            out.push(text("\u{F0450}".into(), x, y, if hovered { fg } else { faint }, false));
        }

        match setting.control {
            Control::Toggle => {
                let on = matches!(value, Value::Bool(true));
                let r = row.parts[0].1;
                out.push(rounded(r, r.height / 2.0, if on { accent } else { control_bg }));
                let knob = r.height - px(6.0);
                let kx = if on { r.x + r.width - knob - px(3.0) } else { r.x + px(3.0) };
                out.push(rounded(Rect::new(kx, r.y + px(3.0), knob, knob), knob / 2.0, if on { readable_on(accent) } else { muted }));
            }
            Control::Choice(options) => {
                let first = row.parts[0].1;
                let last = row.parts[row.parts.len() - 1].1;
                out.push(rounded(Rect::new(first.x, first.y, last.x + last.width - first.x, first.height), px(6.0), control_bg));
                for (i, (target, r)) in row.parts.iter().enumerate() {
                    let selected = value == Value::Text(options[i].0.to_string());
                    if selected {
                        out.push(rounded(Rect::new(r.x + px(2.0), r.y + px(2.0), r.width - px(4.0), r.height - px(4.0)), px(5.0), accent));
                    } else if page.hover == Some(*target) {
                        out.push(rounded(Rect::new(r.x + px(2.0), r.y + px(2.0), r.width - px(4.0), r.height - px(4.0)), px(5.0), mix(control_bg, fg, 0.1)));
                    }
                    let label = options[i].1;
                    let (x, y) = center(*r, text_columns(label));
                    out.push(text(label.into(), x, y, if selected { readable_on(accent) } else { fg }, selected));
                }
            }
            Control::Number { decimals, unit, .. } => {
                let shown = match value {
                    Value::Number(n) => format!("{n:.decimals$} {unit}").trim().to_string(),
                    _ => String::new(),
                };
                draw_stepper(row, &shown, page, out, &text, &rounded, &center, control_bg, fg, px(6.0));
            }
            Control::Optional { unset, .. } => {
                let shown = match value {
                    Value::Number(n) => format!("{:.0}%", n * 100.0),
                    _ => unset.to_string(),
                };
                draw_stepper(row, &shown, page, out, &text, &rounded, &center, control_bg, fg, px(6.0));
            }
            Control::Text { placeholder } => {
                let r = row.parts[0].1;
                let typing = focused && page.typing.is_some();
                if typing {
                    out.push(rounded(Rect::new(r.x - px(1.0), r.y - px(1.0), r.width + px(2.0), r.height + px(2.0)), px(7.0), accent));
                }
                out.push(rounded(r, px(6.0), control_bg));
                let cols = ((r.width - px(20.0)) / m.width) as usize;
                let (shown, color) = match (&page.typing, &value) {
                    (Some(t), _) if typing => {
                        let chars: Vec<char> = t.chars().collect();
                        (chars[chars.len().saturating_sub(cols.saturating_sub(1))..].iter().collect::<String>(), fg)
                    }
                    (_, Value::Text(t)) if !t.is_empty() => (truncate(t, cols), fg),
                    _ => (truncate(placeholder, cols), faint),
                };
                let y = r.y + ((r.height - m.height) / 2.0).round();
                let x = r.x + px(10.0);
                if typing {
                    out.push(ChromeItem::Rect {
                        rect: [x + text_columns(&shown) as f32 * m.width, y, px(2.0), m.height],
                        color: accent,
                        alpha: 1.0,
                    });
                }
                out.push(text(shown, x, y, color, false));
            }
            Control::Font => {
                let r = row.parts[0].1;
                let hovered = page.hover == Some(row.parts[0].0);
                out.push(rounded(r, px(6.0), if hovered { mix(control_bg, fg, 0.08) } else { control_bg }));
                let family = match &value {
                    Value::Text(t) => t.clone(),
                    _ => String::new(),
                };
                let y = r.y + ((r.height - m.height) / 2.0).round();
                out.push(ChromeItem::FontText {
                    text: family,
                    x: r.x + px(10.0),
                    y,
                    color: fg,
                    family: match &value {
                        Value::Text(t) => t.clone(),
                        _ => String::new(),
                    },
                });
                out.push(text("\u{F078}".into(), r.x + r.width - px(10.0) - m.width, y, muted, false));
            }
            Control::Edit(_) => {
                let r = row.parts[0].1;
                let hovered = page.hover == Some(row.parts[0].0);
                out.push(rounded(r, px(6.0), if hovered { mix(bg, accent, 0.35) } else { mix(bg, accent, 0.2) }));
                let label = format!("\u{F044}  Edit in {}", ctx.editor_name);
                let label = truncate(&label, ((r.width - px(12.0)) / m.width) as usize);
                let (x, y) = center(r, text_columns(&label));
                out.push(text(label, x, y, fg, false));
            }
        }
    }

    // "More below/above" hints when the section scrolls.
    if page.scroll > 0
        && let Some(first) = layout.rows.first()
    {
        out.push(text("\u{F0143}".into(), first.rect.x + first.rect.width - m.width, first.rect.y - m.height, faint, false));
    }
    if page.scroll + layout.visible < settings.len()
        && let Some(last) = layout.rows.last()
    {
        out.push(text("\u{F0140}".into(), last.rect.x + last.rect.width - m.width, last.rect.y + last.rect.height, faint, false));
    }

    if let (Some(picker), Some(pl)) = (&page.picker, &layout.picker) {
        out.push(ChromeItem::Rect {
            rect: [a.x, a.y, a.width, a.height],
            color: Rgb::new(0, 0, 0),
            alpha: 0.35,
        });
        out.push(rounded(Rect::new(pl.panel.x - px(1.0), pl.panel.y - px(1.0), pl.panel.width + px(2.0), pl.panel.height + px(2.0)), px(11.0), mix(bg, fg, 0.2)));
        out.push(rounded(pl.panel, px(10.0), mix(bg, fg, 0.06)));
        out.push(rounded(pl.search, px(6.0), control_bg));
        let y = pl.search.y + ((pl.search.height - m.height) / 2.0).round();
        let (shown, color) = if picker.filter.is_empty() { ("Search fonts".to_string(), faint) } else { (picker.filter.clone(), fg) };
        out.push(text(format!("\u{F0349}  {shown}"), pl.search.x + px(10.0), y, color, false));
        let fonts = picker.shown();
        for (i, r) in &pl.rows {
            let Some(family) = fonts.get(*i) else { continue };
            if *i == picker.selected {
                out.push(rounded(*r, px(6.0), mix(bg, accent, 0.3)));
            } else if page.hover == Some(Target::PickerRow(*i)) {
                out.push(rounded(*r, px(6.0), control_bg));
            }
            let current = value_of(ctx.config, &SECTIONS[0].settings[0]) == Value::Text(family.to_string());
            let y = r.y + ((r.height - m.height) / 2.0).round();
            out.push(ChromeItem::FontText {
                text: (*family).clone(),
                x: r.x + px(10.0),
                y,
                color: fg,
                family: (*family).clone(),
            });
            if current {
                out.push(text("\u{F012C}".into(), r.x + r.width - px(10.0) - m.width, y, accent, false));
            }
        }
        if fonts.is_empty() {
            out.push(text("No font matches".into(), pl.search.x + px(10.0), pl.search.y + pl.search.height * 1.5, muted, false));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_stepper(
    row: &RowLayout,
    shown: &str,
    page: &SettingsPage,
    out: &mut Vec<ChromeItem>,
    text: &dyn Fn(String, f32, f32, Rgb, bool) -> ChromeItem,
    rounded: &dyn Fn(Rect, f32, Rgb) -> ChromeItem,
    center: &dyn Fn(Rect, usize) -> (f32, f32),
    control_bg: Rgb,
    fg: Rgb,
    radius: f32,
) {
    for (target, r) in &row.parts {
        let hovered = page.hover == Some(*target);
        let label = match target {
            Target::Minus(_) => "−".to_string(),
            Target::Plus(_) => "+".to_string(),
            _ => shown.to_string(),
        };
        out.push(rounded(*r, radius, if hovered && !matches!(target, Target::Row(_)) { mix(control_bg, fg, 0.12) } else { control_bg }));
        let (x, y) = center(*r, text_columns(&label));
        out.push(text(label, x, y, fg, false));
    }
}

/// The row a target belongs to.
fn target_row(target: Target) -> Option<usize> {
    match target {
        Target::Row(r) | Target::Toggle(r) | Target::Choice(r, _) | Target::Minus(r) | Target::Plus(r) | Target::Text(r) | Target::Button(r) | Target::Reset(r) => Some(r),
        Target::Nav(_) | Target::PickerRow(_) => None,
    }
}

//#endregion

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_has_a_value() {
        let config = Config::default();
        for section in SECTIONS {
            for setting in section.settings {
                let value = value_of(&config, setting);
                match setting.control {
                    Control::Edit(_) => assert_eq!(value, Value::Unset),
                    Control::Optional { .. } => {}
                    _ => assert_ne!(value, Value::Unset, "{}.{}", setting.section, setting.key),
                }
            }
        }
    }

    #[test]
    fn steps_and_toggles_write_minimal_changes() {
        let config = Config::default();
        let mut page = SettingsPage::new();
        // Font size 11 → 11.5, written as a float.
        assert_eq!(
            page.step(1, 1, &config),
            Some(Request::Set {
                section: "font",
                key: "size",
                value: Some(SettingValue::Float(11.5))
            })
        );
        // Ligatures on → off; back on would be the default, so it's removed.
        assert_eq!(
            page.activate(2, &config),
            Some(Request::Set {
                section: "font",
                key: "ligatures",
                value: Some(SettingValue::Bool(false))
            })
        );
        assert_eq!(
            page.step(2, 1, &config),
            Some(Request::Set {
                section: "font",
                key: "ligatures",
                value: None
            })
        );
        // Opacity: unset (theme) steps down into numbers.
        assert_eq!(
            page.step(5, -1, &config),
            Some(Request::Set {
                section: "window",
                key: "opacity",
                value: Some(SettingValue::Float(1.0))
            })
        );
        // Typing into the shell field, then Enter.
        page.select_section(2);
        assert_eq!(page.key(&Key::Named(NamedKey::Enter), None, &config), None);
        page.key(&Key::Character("f".into()), Some("fish"), &config);
        assert_eq!(
            page.key(&Key::Named(NamedKey::Enter), None, &config),
            Some(Request::Set {
                section: "terminal",
                key: "shell",
                value: Some(SettingValue::Text("fish".into()))
            })
        );
    }
}
