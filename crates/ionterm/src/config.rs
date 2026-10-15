//! `ionterm.toml`: the user's settings. Every key has a default, so ionterm
//! runs without a config file at all.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::layout::Layout;

pub const CONFIG_FILE: &str = "ionterm.toml";
pub const THEME_FILE: &str = "theme.toml";

/// `~/.config/ionterm` on Linux, `%APPDATA%\ionterm` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("ionterm"))
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
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            padding: 8.0,
            decorations: Decorations::Auto,
            opacity: None,
            columns: 100,
            lines: 30,
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
    /// Program and arguments from `ionterm -e`; overrides `shell`.
    #[serde(skip)]
    pub command: Vec<String>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            scrollback_lines: 10_000,
            shell: String::new(),
            term: "auto".into(),
            wheel_scroll_lines: 3,
            advertise_sixel: false,
            command: Vec::new(),
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
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            shape: CursorShapeConfig::Beam,
            blink_interval: 0.5,
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
