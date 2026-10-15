//! `ionterm.toml`: the user's settings. Every key has a default, so ionterm
//! runs without a config file at all.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

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
            command: Vec::new(),
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
