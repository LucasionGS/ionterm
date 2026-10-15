//! `ionterm.toml`: the user's settings. Every key has a default, so ionterm
//! runs without a config file at all.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::layout::Layout;

pub const CONFIG_FILE: &str = "ionterm.toml";
pub const THEME_FILE: &str = "theme.toml";

/// `~/.config/ionterm` on Linux, `%APPDATA%\ionterm` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("ionterm"))
}

/// A value the settings page writes into `ionterm.toml`.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

/// Sets `section.key` in `ionterm.toml` (or removes it, back to the default,
/// with `None`), leaving the rest of the file, comments included, as it was.
/// Creates the file when needed.
pub fn write_setting(section: &str, key: &str, value: Option<SettingValue>) -> std::io::Result<()> {
    let dir = config_dir().ok_or_else(|| std::io::Error::other("no config folder"))?;
    write_setting_in(&dir.join(CONFIG_FILE), section, key, value)
}

pub fn write_setting_in(path: &Path, section: &str, key: &str, value: Option<SettingValue>) -> std::io::Result<()> {
    let (text, header) = match std::fs::read_to_string(path) {
        Ok(text) => (text, ""),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if value.is_none() {
                return Ok(());
            }
            (String::new(), "# ionterm settings. Every option, with its default: extras/ionterm.toml in the ionterm repository.\n\n")
        }
        Err(error) => return Err(error),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| std::io::Error::other(e.to_string()))?;
    match value {
        Some(value) => {
            let table = doc
                .entry(section)
                .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
                .as_table_like_mut()
                .ok_or_else(|| std::io::Error::other(format!("[{section}] in {} isn't a table", path.display())))?;
            let value = match value {
                SettingValue::Bool(b) => toml_edit::value(b),
                SettingValue::Int(i) => toml_edit::value(i),
                // Rounded, so stepping by 0.1 doesn't write 0.30000000000000004.
                SettingValue::Float(f) => toml_edit::value((f * 1000.0).round() / 1000.0),
                SettingValue::Text(t) => toml_edit::value(t),
            };
            table.insert(key, value);
        }
        None => {
            if let Some(table) = doc.get_mut(section).and_then(|t| t.as_table_like_mut()) {
                table.remove(key);
                if table.is_empty() {
                    doc.remove(section);
                }
            }
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{header}{doc}"))
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub font: FontConfig,
    pub window: WindowConfig,
    pub terminal: TerminalConfig,
    pub cursor: CursorConfig,
    pub tab_bar: TabBarConfig,
    pub layouts: LayoutConfig,
    pub files: FilesConfig,
    pub hints: HintsConfig,
    pub notifications: NotificationsConfig,
    pub session: SessionConfig,
    /// Key combo → action name, merged over the defaults. `"none"` unbinds.
    pub keybindings: HashMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct FontConfig {
    /// Empty = the system monospace font.
    pub family: String,
    /// In points.
    pub size: f32,
    /// Programming ligatures (`->`, `!=`, `===`) when the font has them.
    pub ligatures: bool,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "JetBrainsMono Nerd Font".into(),
            size: 11.0,
            ligatures: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Decorations {
    /// None under a tiling compositor (Hyprland), the platform's otherwise.
    #[default]
    Auto,
    None,
    Full,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct WindowConfig {
    /// Space around the grid, in logical pixels.
    pub padding: f32,
    pub decorations: Decorations,
    /// Overrides the theme's `background_opacity` when set.
    pub opacity: Option<f32>,
    /// Initial size in cells.
    pub columns: u16,
    pub lines: u16,
    /// A picture behind the text (PNG, JPEG or WebP; `~` is your home).
    /// Empty = none.
    pub background_image: String,
    /// How strongly the picture shows through the background colour, 0–1.
    pub background_image_opacity: f32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            padding: 8.0,
            decorations: Decorations::Auto,
            opacity: None,
            columns: 100,
            lines: 30,
            background_image: String::new(),
            background_image_opacity: 0.25,
        }
    }
}

/// Desktop notifications.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct NotificationsConfig {
    /// Seconds a command must run before its end is announced when you're
    /// elsewhere (another tab, another window). 0 turns it off.
    pub command_done_after: f32,
    /// Let programs send notifications (OSC 9 and OSC 777).
    pub from_programs: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            command_done_after: 10.0,
            from_programs: true,
        }
    }
}

/// Reopening the last window's tabs.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct SessionConfig {
    /// Reopen the tabs, splits, directories and SSH connections the last
    /// window had when it was closed.
    pub restore: bool,
}

/// Hints mode (ctrl+shift+e).
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct HintsConfig {
    /// The command that opens a picked file, with `{path}`, `{line}` and
    /// `{col}` filled in (`code -g {path}:{line}`). Empty = `$VISUAL` or
    /// `$EDITOR` in a new tab.
    pub open_file: String,
}

/// The file panel of SSH tabs.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct FilesConfig {
    /// Panel width in columns (at most half the window).
    pub panel_width: u16,
    /// Where downloads go. Empty = the system's Downloads folder.
    pub download_dir: String,
    /// Show dotfiles when the panel opens.
    pub show_hidden: bool,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            panel_width: 46,
            download_dir: String::new(),
            show_hidden: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct TerminalConfig {
    pub scrollback_lines: usize,
    /// Command line to run. Empty = the user's login shell.
    pub shell: String,
    /// `TERM` for the child. `"auto"` = `xterm-ionterm` when its terminfo is
    /// installed, `xterm-256color` otherwise.
    pub term: String,
    /// Lines moved per mouse wheel notch.
    pub wheel_scroll_lines: usize,
    /// Announce sixel support to programs. Sixel images display either way;
    /// announcing it makes some (yazi) prefer sixel over the sharper kitty
    /// protocol, but others (lsix) need it.
    pub advertise_sixel: bool,
    /// Set up bash, zsh and fish 3 to mark prompts and commands (OSC 133),
    /// for jumping between prompts, selecting a command's output and
    /// failed-command marks. Only the login shell; fish 4 does it itself.
    pub shell_integration: bool,
    /// Ask before pasting several lines into a program that would run each
    /// one as it arrives (no bracketed paste).
    pub confirm_multiline_paste: bool,
    /// Shows the scrollback (`show_scrollback`); `INPUT_LINE_NUMBER` becomes
    /// the line to open at, and the file's path goes last.
    pub scrollback_pager: String,
    /// Listen for `ionterm @` commands from programs running as you.
    pub remote_control: bool,
    /// Program and arguments from `ionterm -e`; overrides `shell`.
    #[serde(skip)]
    pub command: Vec<String>,
    /// Host alias from `ionterm ssh <host>`: the first tab connects to it.
    #[serde(skip)]
    pub startup_ssh: Option<String>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback_lines: 10_000,
            shell: String::new(),
            term: "auto".into(),
            wheel_scroll_lines: 3,
            advertise_sixel: false,
            shell_integration: true,
            confirm_multiline_paste: true,
            scrollback_pager: "less -R +INPUT_LINE_NUMBER".into(),
            remote_control: true,
            command: Vec::new(),
            startup_ssh: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TabBarEdge {
    #[default]
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TabBarStyleConfig {
    /// Windows Terminal-style rounded tabs with close and new-tab buttons.
    #[default]
    Tabs,
    /// kitty-style Powerline strips.
    Angled,
    Slanted,
    Round,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ShowTabBar {
    /// Only with two or more tabs.
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct TabBarConfig {
    pub edge: TabBarEdge,
    pub style: TabBarStyleConfig,
    pub show: ShowTabBar,
    /// Longer titles are cut with an ellipsis (Powerline styles; the tabs
    /// style fits titles to the tab width).
    pub max_title_length: usize,
}

impl Default for TabBarConfig {
    fn default() -> Self {
        Self {
            edge: TabBarEdge::Top,
            style: TabBarStyleConfig::Tabs,
            show: ShowTabBar::Auto,
            max_title_length: 30,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct LayoutConfig {
    /// Layouts `next_layout` cycles through, in order. The first is used for new tabs.
    pub enabled: Vec<Layout>,
    /// Border between panes, in logical pixels.
    pub border_width: f32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            enabled: vec![Layout::Splits, Layout::Tall, Layout::Fat, Layout::Grid, Layout::Stack],
            border_width: 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CursorShapeConfig {
    Block,
    #[default]
    Beam,
    Underline,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct CursorConfig {
    pub shape: CursorShapeConfig,
    /// Seconds per blink phase; 0 disables blinking.
    pub blink_interval: f32,
    /// A fading streak behind the cursor when it jumps.
    pub trail: bool,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            shape: CursorShapeConfig::Beam,
            blink_interval: 0.5,
            trail: false,
        }
    }
}

impl Config {
    /// Loads the config, falling back to defaults (with a logged warning) when
    /// the file is missing or invalid, so a typo never stops the terminal opening.
    pub fn load() -> Self {
        let Some(path) = config_dir().map(|dir| dir.join(CONFIG_FILE)) else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(config) => config,
                Err(error) => {
                    tracing::warn!("ignoring {}: {error}", path.display());
                    Self::default()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tracing::warn!("could not read {}: {error}", path.display());
                Self::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_settings_keeping_the_rest() {
        let path = std::env::temp_dir().join(format!("ionterm-config-{}.toml", std::process::id()));
        std::fs::write(&path, "# mine\n[font]\nsize = 11.0 # comfy\nfamily = \"Iosevka\"\n").unwrap();
        write_setting_in(&path, "font", "size", Some(SettingValue::Float(12.300000001))).unwrap();
        write_setting_in(&path, "cursor", "shape", Some(SettingValue::Text("block".into()))).unwrap();
        write_setting_in(&path, "font", "family", None).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mine\n[font]\nsize = 12.3"), "{text}");
        assert!(!text.contains("Iosevka") && text.contains("[cursor]\nshape = \"block\""), "{text}");
        // Removing the last key of a section drops the section.
        write_setting_in(&path, "cursor", "shape", None).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("[cursor]"));
        let parsed: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.font.size, 12.3);
        let _ = std::fs::remove_file(path);
    }
}
