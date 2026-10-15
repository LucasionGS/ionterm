//! Turning keyboard and mouse input into the bytes applications expect.
//!
//! Keys use the legacy xterm encoding until an application enables the kitty
//! keyboard protocol (`CSI > flags u`), after which they are encoded per
//! <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>.

use ionterm_vt::TermMode;
use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

//#region Kitty keyboard flags

const DISAMBIGUATE: u8 = 1;
const REPORT_EVENT_TYPES: u8 = 2;
const REPORT_ALTERNATES: u8 = 4;
const REPORT_ALL_AS_ESCAPES: u8 = 8;
const REPORT_TEXT: u8 = 16;

//#endregion

/// xterm-style modifier parameter bits (the encoded value is `1 + bits`).
fn modifier_bits(mods: ModifiersState) -> u8 {
    let mut bits = 0;
    if mods.shift_key() {
        bits |= 1;
    }
    if mods.alt_key() {
        bits |= 2;
    }
    if mods.control_key() {
        bits |= 4;
    }
    if mods.super_key() {
        bits |= 8;
    }
    bits
}

/// How a non-text key is written: `CSI <number> ; <mods> <final>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Functional {
    number: u32,
    terminator: char,
}

const fn func(number: u32, terminator: char) -> Option<Functional> {
    Some(Functional { number, terminator })
}

/// The kitty encoding of a functional key. Legacy mode reuses the same table
/// except for the handful of keys that differ there.
fn functional_key(event: &KeyEvent) -> Option<Functional> {
    let Key::Named(named) = &event.logical_key else {
        return None;
    };
    match named {
        NamedKey::Escape => func(27, 'u'),
        NamedKey::Enter => func(13, 'u'),
        NamedKey::Tab => func(9, 'u'),
        NamedKey::Backspace => func(127, 'u'),
        NamedKey::Insert => func(2, '~'),
        NamedKey::Delete => func(3, '~'),
        NamedKey::ArrowLeft => func(1, 'D'),
        NamedKey::ArrowRight => func(1, 'C'),
        NamedKey::ArrowUp => func(1, 'A'),
        NamedKey::ArrowDown => func(1, 'B'),
        NamedKey::PageUp => func(5, '~'),
        NamedKey::PageDown => func(6, '~'),
        NamedKey::Home => func(1, 'H'),
        NamedKey::End => func(1, 'F'),
        NamedKey::CapsLock => func(57358, 'u'),
        NamedKey::ScrollLock => func(57359, 'u'),
        NamedKey::NumLock => func(57360, 'u'),
        NamedKey::PrintScreen => func(57361, 'u'),
        NamedKey::Pause => func(57362, 'u'),
        NamedKey::ContextMenu => func(57363, 'u'),
        NamedKey::F1 => func(1, 'P'),
        NamedKey::F2 => func(1, 'Q'),
        NamedKey::F3 => func(13, '~'),
        NamedKey::F4 => func(1, 'S'),
        NamedKey::F5 => func(15, '~'),
        NamedKey::F6 => func(17, '~'),
        NamedKey::F7 => func(18, '~'),
        NamedKey::F8 => func(19, '~'),
        NamedKey::F9 => func(20, '~'),
        NamedKey::F10 => func(21, '~'),
        NamedKey::F11 => func(23, '~'),
        NamedKey::F12 => func(24, '~'),
        NamedKey::Shift | NamedKey::Control | NamedKey::Alt | NamedKey::Super | NamedKey::Meta | NamedKey::Hyper => {
            let right = matches!(
                event.physical_key,
                PhysicalKey::Code(
                    KeyCode::ShiftRight | KeyCode::ControlRight | KeyCode::AltRight | KeyCode::SuperRight
                )
            );
            let base = match named {
                NamedKey::Shift => 57441,
                NamedKey::Control => 57442,
                NamedKey::Alt => 57443,
                NamedKey::Super => 57444,
                NamedKey::Hyper => 57445,
                _ => 57446,
            };
            func(if right { base + 6 } else { base }, 'u')
        }
        _ => None,
    }
}

fn is_modifier_key(f: Functional) -> bool {
    (57441..=57452).contains(&f.number)
}

/// The key's character with no modifiers applied, lowercased (kitty's "base key").
fn base_char(event: &KeyEvent) -> Option<char> {
    match event.key_without_modifiers() {
        Key::Character(s) => {
            let mut chars = s.chars();
            let c = chars.next()?;
            chars.next().is_none().then(|| c.to_lowercase().next().unwrap_or(c))
        }
        Key::Named(NamedKey::Space) => Some(' '),
        _ => None,
    }
}

/// Encodes a key event for the PTY. Returns `None` when nothing is sent.
pub fn encode_key(event: &KeyEvent, mods: ModifiersState, mode: TermMode, kitty_flags: u8) -> Option<Vec<u8>> {
    if kitty_flags & (DISAMBIGUATE | REPORT_EVENT_TYPES | REPORT_ALTERNATES | REPORT_ALL_AS_ESCAPES | REPORT_TEXT) == 0 {
        if event.state != ElementState::Pressed {
            return None;
        }
        return encode_legacy(event, mods, mode);
    }
    encode_kitty(event, mods, kitty_flags)
}

fn encode_legacy(event: &KeyEvent, mods: ModifiersState, mode: TermMode) -> Option<Vec<u8>> {
    let bits = modifier_bits(mods);
    let alt_prefix = |mut bytes: Vec<u8>| {
        if mods.alt_key() {
            bytes.insert(0, 0x1b);
        }
        bytes
    };

    if let Key::Named(named) = &event.logical_key {
        match named {
            NamedKey::Enter => return Some(alt_prefix(b"\r".to_vec())),
            NamedKey::Tab if mods.shift_key() => return Some(b"\x1b[Z".to_vec()),
            NamedKey::Tab => return Some(alt_prefix(b"\t".to_vec())),
            NamedKey::Backspace => {
                let byte = if mods.control_key() { 0x08 } else { 0x7f };
                return Some(alt_prefix(vec![byte]));
            }
            NamedKey::Escape => return Some(alt_prefix(vec![0x1b])),
            NamedKey::Space if mods.control_key() => return Some(alt_prefix(vec![0])),
            _ => {}
        }
        if let Some(f) = functional_key(event)
            && f.terminator != 'u'
        {
            // F1–F4 are SS3 sequences when unmodified, and F3 uses `R` in legacy mode.
            let (number, terminator) = match (named, f.terminator) {
                (NamedKey::F3, _) => (1, 'R'),
                other => (f.number, other.1),
            };
            let s = if terminator == '~' {
                if bits == 0 {
                    format!("\x1b[{number}~")
                } else {
                    format!("\x1b[{number};{}~", bits + 1)
                }
            } else if bits != 0 {
                format!("\x1b[1;{}{terminator}", bits + 1)
            } else if matches!(terminator, 'P' | 'Q' | 'R' | 'S')
                || (mode.contains(TermMode::APP_CURSOR) && matches!(terminator, 'A' | 'B' | 'C' | 'D' | 'H' | 'F'))
            {
                format!("\x1bO{terminator}")
            } else {
                format!("\x1b[{terminator}")
            };
            return Some(s.into_bytes());
        }
    }

    if mods.control_key()
        && let Some(c) = base_char(event)
    {
        let control = match c {
            'a'..='z' => Some(c as u8 - b'a' + 1),
            '@' | '2' | ' ' => Some(0),
            '[' | '3' => Some(0x1b),
            '\\' | '4' => Some(0x1c),
            ']' | '5' => Some(0x1d),
            '^' | '6' => Some(0x1e),
            '_' | '7' | '/' | '-' => Some(0x1f),
            '8' | '?' => Some(0x7f),
            _ => None,
        };
        if let Some(byte) = control {
            return Some(alt_prefix(vec![byte]));
        }
    }

    let text = event.text.as_deref().filter(|t| !t.is_empty() && !t.chars().all(char::is_control));
    // With alt held, the text is the key itself (not a compose result).
    let text = match (text, mods.alt_key()) {
        (Some(t), _) => t.to_string(),
        (None, true) => base_char(event)?.to_string(),
        (None, false) => return None,
    };
    Some(alt_prefix(text.into_bytes()))
}

fn encode_kitty(event: &KeyEvent, mods: ModifiersState, flags: u8) -> Option<Vec<u8>> {
    let pressed = event.state == ElementState::Pressed;
    let report_types = flags & REPORT_EVENT_TYPES != 0;
    let all_escapes = flags & REPORT_ALL_AS_ESCAPES != 0;
    if !pressed && !report_types {
        return None;
    }
    let event_type = match (pressed, event.repeat) {
        (false, _) => 3,
        (true, true) => 2,
        (true, false) => 1,
    };
    let bits = modifier_bits(mods);
    let text = event.text.as_deref().filter(|t| !t.is_empty() && !t.chars().any(char::is_control));

    let functional = functional_key(event);
    if let Some(f) = functional {
        if is_modifier_key(f) && !all_escapes {
            return None;
        }
        // Enter, Tab and Backspace stay legacy unless modified, so a shell
        // left in this mode by a crashed program is still usable.
        if !all_escapes && bits == 0 && matches!(f.number, 13 | 9 | 127) && f.terminator == 'u' {
            if !pressed {
                return None;
            }
            return Some(vec![match f.number {
                13 => b'\r',
                9 => b'\t',
                _ => 0x7f,
            }]);
        }
    }

    let (number, terminator, alternate) = match functional {
        Some(f) => (f.number, f.terminator, None),
        None => {
            let base = base_char(event)?;
            // Plain typing stays plain text unless every key must be an escape.
            if !all_escapes && bits & !1 == 0 && text.is_some() {
                if !pressed {
                    return None;
                }
                return text.map(|t| t.as_bytes().to_vec());
            }
            let shifted = (flags & REPORT_ALTERNATES != 0 && mods.shift_key())
                .then(|| match &event.logical_key {
                    Key::Character(s) => s.chars().next().filter(|&c| c != base),
                    _ => None,
                })
                .flatten();
            (base as u32, 'u', shifted)
        }
    };

    let mut s = String::from("\x1b[");
    // Only keys that type something carry text; ctrl+a or alt+x don't.
    let text_field = (flags & REPORT_TEXT != 0 && all_escapes && pressed && bits & !1 == 0)
        .then(|| text.map(|t| t.chars().map(|c| (c as u32).to_string()).collect::<Vec<_>>().join(":")))
        .flatten();
    let needs_mods = bits != 0 || (report_types && event_type != 1) || text_field.is_some();
    if number != 1 || terminator == 'u' || terminator == '~' || needs_mods {
        s.push_str(&number.to_string());
    }
    if let Some(alt) = alternate {
        s.push_str(&format!(":{}", alt as u32));
    }
    if needs_mods {
        s.push_str(&format!(";{}", bits + 1));
        if report_types && event_type != 1 {
            s.push_str(&format!(":{event_type}"));
        }
    }
    if let Some(text) = text_field {
        s.push_str(&format!(";{text}"));
    }
    s.push(terminator);
    Some(s.into_bytes())
}

//#region Mouse

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    /// Motion; `Some` while a button is held.
    Motion(Option<MouseButton>),
}

/// Encodes a mouse event for an application that enabled mouse reporting.
/// Returns `None` if the active tracking mode doesn't want this event.
pub fn encode_mouse(
    action: MouseAction,
    button: Option<MouseButton>,
    col: usize,
    row: usize,
    mods: ModifiersState,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let wants = match action {
        MouseAction::Press => mode.intersects(TermMode::MOUSE_REPORTING),
        MouseAction::Release => mode.intersects(TermMode::MOUSE_NORMAL | TermMode::MOUSE_BUTTON | TermMode::MOUSE_ANY),
        MouseAction::Motion(Some(_)) => mode.intersects(TermMode::MOUSE_BUTTON | TermMode::MOUSE_ANY),
        MouseAction::Motion(None) => mode.contains(TermMode::MOUSE_ANY),
    };
    if !wants {
        return None;
    }

    let button = match action {
        MouseAction::Motion(held) => held,
        _ => button,
    };
    let mut code: u32 = match button {
        Some(MouseButton::Left) => 0,
        Some(MouseButton::Middle) => 1,
        Some(MouseButton::Right) => 2,
        Some(MouseButton::WheelUp) => 64,
        Some(MouseButton::WheelDown) => 65,
        None => 3,
    };
    if !mode.contains(TermMode::MOUSE_X10) {
        if mods.shift_key() {
            code += 4;
        }
        if mods.alt_key() {
            code += 8;
        }
        if mods.control_key() {
            code += 16;
        }
    }
    if matches!(action, MouseAction::Motion(_)) {
        code += 32;
    }

    if mode.contains(TermMode::MOUSE_SGR) {
        let end = if action == MouseAction::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{};{}{end}", col + 1, row + 1).into_bytes());
    }
    if action == MouseAction::Release {
        // The legacy encoding can't say which button was released.
        code = 3 + (code & !3 & !64);
    }
    if mode.contains(TermMode::MOUSE_URXVT) {
        return Some(format!("\x1b[{};{};{}M", code + 32, col + 1, row + 1).into_bytes());
    }
    // X10 encoding: one byte per value, so coordinates past 222 can't be sent.
    if col > 222 || row > 222 {
        return None;
    }
    Some(vec![0x1b, b'[', b'M', (code + 32) as u8, (col + 33) as u8, (row + 33) as u8])
}

//#endregion

//#region Keybindings

/// A key combination like `ctrl+shift+c`, normalised for matching.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub mods: u8,
    pub key: String,
}

impl KeyCombo {
    pub fn parse(s: &str) -> Option<Self> {
        let mut mods = 0;
        let mut key = None;
        for part in s.to_lowercase().split('+').filter(|p| !p.is_empty()) {
            match part {
                "shift" => mods |= 1,
                "alt" | "opt" | "option" => mods |= 2,
                "ctrl" | "control" => mods |= 4,
                "super" | "cmd" | "win" => mods |= 8,
                other => key = Some(normalize_key_name(other)),
            }
        }
        // `ctrl+shift++` splits into an empty last part.
        if key.is_none() && s.ends_with("++") {
            key = Some("plus".into());
        }
        Some(Self { mods, key: key? })
    }

    pub fn from_event(event: &KeyEvent, mods: ModifiersState) -> Option<Self> {
        let key = match event.key_without_modifiers() {
            Key::Character(s) => normalize_key_name(&s.to_lowercase()),
            Key::Named(named) => named_key_name(named)?,
            _ => return None,
        };
        Some(Self {
            mods: modifier_bits(mods),
            key,
        })
    }
}

fn normalize_key_name(name: &str) -> String {
    match name {
        "+" => "plus",
        "-" => "minus",
        "=" => "equal",
        "," => "comma",
        "." => "period",
        "/" => "slash",
        "\\" => "backslash",
        "[" => "left_bracket",
        "]" => "right_bracket",
        ";" => "semicolon",
        "'" => "apostrophe",
        "`" => "grave",
        " " => "space",
        "return" => "enter",
        "esc" => "escape",
        "pageup" | "pgup" => "page_up",
        "pagedown" | "pgdn" => "page_down",
        "del" => "delete",
        "ins" => "insert",
        other => other,
    }
    .to_string()
}

fn named_key_name(named: NamedKey) -> Option<String> {
    let name = match named {
        NamedKey::Enter => "enter",
        NamedKey::Tab => "tab",
        NamedKey::Space => "space",
        NamedKey::Backspace => "backspace",
        NamedKey::Escape => "escape",
        NamedKey::Insert => "insert",
        NamedKey::Delete => "delete",
        NamedKey::Home => "home",
        NamedKey::End => "end",
        NamedKey::PageUp => "page_up",
        NamedKey::PageDown => "page_down",
        NamedKey::ArrowUp => "up",
        NamedKey::ArrowDown => "down",
        NamedKey::ArrowLeft => "left",
        NamedKey::ArrowRight => "right",
        NamedKey::F1 => "f1",
        NamedKey::F2 => "f2",
        NamedKey::F3 => "f3",
        NamedKey::F4 => "f4",
        NamedKey::F5 => "f5",
        NamedKey::F6 => "f6",
        NamedKey::F7 => "f7",
        NamedKey::F8 => "f8",
        NamedKey::F9 => "f9",
        NamedKey::F10 => "f10",
        NamedKey::F11 => "f11",
        NamedKey::F12 => "f12",
        _ => return None,
    };
    Some(name.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Copy,
    Paste,
    PasteSelection,
    ScrollLineUp,
    ScrollLineDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollHome,
    ScrollEnd,
    IncreaseFontSize,
    DecreaseFontSize,
    ResetFontSize,
    ReloadConfig,
    /// Unbinds a default.
    None,
}

impl Action {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "copy" | "copy_to_clipboard" => Self::Copy,
            "paste" | "paste_from_clipboard" => Self::Paste,
            "paste_selection" | "paste_from_selection" => Self::PasteSelection,
            "scroll_line_up" => Self::ScrollLineUp,
            "scroll_line_down" => Self::ScrollLineDown,
            "scroll_page_up" => Self::ScrollPageUp,
            "scroll_page_down" => Self::ScrollPageDown,
            "scroll_home" => Self::ScrollHome,
            "scroll_end" => Self::ScrollEnd,
            "increase_font_size" => Self::IncreaseFontSize,
            "decrease_font_size" => Self::DecreaseFontSize,
            "reset_font_size" => Self::ResetFontSize,
            "reload_config" => Self::ReloadConfig,
            "none" => Self::None,
            _ => return None,
        })
    }
}

pub const DEFAULT_BINDINGS: &[(&str, &str)] = &[
    ("ctrl+shift+c", "copy"),
    ("ctrl+shift+v", "paste"),
    ("shift+insert", "paste_selection"),
    ("ctrl+shift+up", "scroll_line_up"),
    ("ctrl+shift+down", "scroll_line_down"),
    ("shift+page_up", "scroll_page_up"),
    ("shift+page_down", "scroll_page_down"),
    ("ctrl+shift+home", "scroll_home"),
    ("ctrl+shift+end", "scroll_end"),
    ("ctrl+shift+equal", "increase_font_size"),
    ("ctrl+shift+plus", "increase_font_size"),
    ("ctrl+shift+minus", "decrease_font_size"),
    ("ctrl+shift+backspace", "reset_font_size"),
    ("ctrl+shift+f5", "reload_config"),
];

/// The defaults with the user's bindings layered on top.
pub fn build_bindings(user: &std::collections::HashMap<String, String>) -> std::collections::HashMap<KeyCombo, Action> {
    let mut bindings = std::collections::HashMap::new();
    let defaults = DEFAULT_BINDINGS.iter().map(|(k, a)| (k.to_string(), a.to_string()));
    for (combo, action) in defaults.chain(user.iter().map(|(k, a)| (k.clone(), a.clone()))) {
        match (KeyCombo::parse(&combo), Action::parse(&action)) {
            (Some(combo), Some(action)) => {
                bindings.insert(combo, action);
            }
            (None, _) => tracing::warn!("ignoring keybinding with no key: {combo:?}"),
            (_, None) => tracing::warn!("ignoring keybinding {combo:?}: unknown action {action:?}"),
        }
    }
    bindings.retain(|_, action| *action != Action::None);
    bindings
}

//#endregion

/// Wraps pasted text for the PTY. Line endings become carriage returns, as if
/// typed, and with bracketed paste the end marker is stripped from the text
/// so a paste can't break out of the bracket and run commands.
pub fn encode_paste(text: &str, mode: TermMode) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if mode.contains(TermMode::BRACKETED_PASTE) {
        let clean = text.replace("\x1b[201~", "").replace('\x1b', "");
        format!("\x1b[200~{clean}\x1b[201~").into_bytes()
    } else {
        text.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_combos() {
        assert_eq!(KeyCombo::parse("ctrl+shift+c"), Some(KeyCombo { mods: 5, key: "c".into() }));
        assert_eq!(KeyCombo::parse("ctrl+shift++"), Some(KeyCombo { mods: 5, key: "plus".into() }));
        assert_eq!(KeyCombo::parse("Shift+PageUp"), Some(KeyCombo { mods: 1, key: "page_up".into() }));
        assert_eq!(KeyCombo::parse("ctrl+shift"), None);
    }

    #[test]
    fn sgr_mouse() {
        let mode = TermMode::MOUSE_NORMAL | TermMode::MOUSE_SGR;
        let press = encode_mouse(MouseAction::Press, Some(MouseButton::Left), 4, 2, ModifiersState::empty(), mode);
        assert_eq!(press.unwrap(), b"\x1b[<0;5;3M");
        let release = encode_mouse(MouseAction::Release, Some(MouseButton::Left), 4, 2, ModifiersState::empty(), mode);
        assert_eq!(release.unwrap(), b"\x1b[<0;5;3m");
        // Plain motion is only reported in any-event mode.
        assert!(encode_mouse(MouseAction::Motion(None), None, 1, 1, ModifiersState::empty(), mode).is_none());
    }

    #[test]
    fn bracketed_paste_is_sanitised() {
        let out = encode_paste("a\nb\x1b[201~rm -rf", TermMode::BRACKETED_PASTE);
        assert_eq!(out, b"\x1b[200~a\rbrm -rf\x1b[201~");
    }
}
