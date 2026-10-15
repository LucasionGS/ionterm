//! `theme.toml`: colours, owned by the Ionix styler.
//!
//! The styler symlinks a per-theme file into `~/.config/ionterm/theme.toml`
//! (rendered from wallpaper colours for adaptive themes). Without one, the
//! static Ionix palette below is used.

use ionterm_vt::{Palette, Rgb};
use serde::{Deserialize, Deserializer};

use crate::config::{THEME_FILE, config_dir};

/// An `#rrggbb` colour in TOML.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeColor(pub Rgb);

impl<'de> Deserialize<'de> for ThemeColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Rgb::from_hex(&s)
            .map(ThemeColor)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid colour {s:?}, expected #rrggbb")))
    }
}

fn hex(s: &str) -> ThemeColor {
    ThemeColor(Rgb::from_hex(s).expect("built-in colours are valid"))
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct Theme {
    pub foreground: ThemeColor,
    pub background: ThemeColor,
    pub background_opacity: f32,
    pub cursor: ThemeColor,
    /// Text under a block cursor. `None` = the background colour.
    pub cursor_text: Option<ThemeColor>,
    /// `None` = keep the text's own colour.
    pub selection_foreground: Option<ThemeColor>,
    pub selection_background: ThemeColor,
    pub url: ThemeColor,
    /// Border of the active pane. `None` = the active tab colour.
    pub active_border: Option<ThemeColor>,
    /// Borders between other panes. `None` = the inactive tab colour.
    pub inactive_border: Option<ThemeColor>,
    pub tab_bar: TabBarTheme,
    pub palette: PaletteTheme,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct TabBarTheme {
    pub background: ThemeColor,
    pub active_foreground: ThemeColor,
    pub active_background: ThemeColor,
    pub inactive_foreground: ThemeColor,
    pub inactive_background: ThemeColor,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct PaletteTheme {
    /// ANSI colours 0–15. Missing entries keep the default.
    pub colors: Vec<ThemeColor>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            foreground: hex("#e2d9f3"),
            background: hex(DEFAULT_BACKGROUND),
            background_opacity: 0.92,
            cursor: hex("#9b59b6"),
            cursor_text: Some(hex(DEFAULT_BACKGROUND)),
            selection_foreground: Some(hex(DEFAULT_BACKGROUND)),
            selection_background: hex("#9b59b6"),
            url: hex("#a855f7"),
            active_border: None,
            inactive_border: None,
            tab_bar: TabBarTheme::default(),
            palette: PaletteTheme::default(),
        }
    }
}

impl Default for TabBarTheme {
    fn default() -> Self {
        Self {
            background: hex(DEFAULT_BACKGROUND),
            active_foreground: hex(DEFAULT_BACKGROUND),
            active_background: hex("#9b59b6"),
            inactive_foreground: hex("#c4b5fd"),
            inactive_background: hex("#1a1025"),
        }
    }
}

impl Default for PaletteTheme {
    fn default() -> Self {
        Self {
            colors: DEFAULT_ANSI.iter().map(|s| hex(s)).collect(),
        }
    }
}

/// The background without a theme, also used for text drawn on the accent
/// colour (cursor, selection, active tab) so it always matches.
const DEFAULT_BACKGROUND: &str = "#0b0b0f";

const DEFAULT_ANSI: [&str; 16] = [
    "#1a1025", "#e74c3c", "#2ecc71", "#f39c12", "#7c3aed", "#9b59b6", "#1abc9c", "#c4b5fd", "#2d2d3f",
    "#ff6b6b", "#55efc4", "#feca57", "#a855f7", "#c084fc", "#48dbfb", "#e2d9f3",
];

impl Theme {
    pub fn load() -> Self {
        let Some(path) = config_dir().map(|dir| dir.join(THEME_FILE)) else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(theme) => theme,
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

    pub fn palette(&self) -> Palette {
        let mut ansi = [Rgb::default(); 16];
        for (i, slot) in ansi.iter_mut().enumerate() {
            let fallback = Rgb::from_hex(DEFAULT_ANSI[i]).expect("built-in colours are valid");
            *slot = self.palette.colors.get(i).map_or(fallback, |c| c.0);
        }
        Palette::with_ansi(ansi, self.foreground.0, self.background.0, self.cursor.0)
    }
}
