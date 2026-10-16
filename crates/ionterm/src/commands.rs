//! The command palette's catalogue (ctrl+shift+p): every action with a
//! readable name, a group and an icon, shown with its current keybinding.

use std::collections::HashMap;

use crate::input::{Action, KeyCombo};

/// (action name, title, group, icon).
const COMMANDS: &[(&str, &str, &str, char)] = &[
    ("new_tab", "New tab", "Tabs", '\u{F0415}'),
    ("close_tab", "Close tab", "Tabs", '\u{F0156}'),
    ("next_tab", "Next tab", "Tabs", '\u{F0142}'),
    ("previous_tab", "Previous tab", "Tabs", '\u{F0141}'),
    ("move_tab_forward", "Move tab right", "Tabs", '\u{F0054}'),
    ("move_tab_backward", "Move tab left", "Tabs", '\u{F004D}'),
    ("rename_tab", "Rename tab", "Tabs", '\u{F03EB}'),
    ("new_os_window", "New window", "Windows", '\u{F05B0}'),
    ("move_tab_to_new_window", "Move tab to a new window", "Windows", '\u{F03CC}'),
    ("move_tab_to_window", "Move tab to window…", "Windows", '\u{F03CC}'),
    ("move_pane_to_new_tab", "Move pane to a new tab", "Panes", '\u{F03CC}'),
    ("move_pane_to_new_window", "Move pane to a new window", "Panes", '\u{F03CC}'),
    ("move_pane", "Move pane to…", "Panes", '\u{F03CC}'),
    ("new_split", "Split pane", "Panes", '\u{F0574}'),
    ("split_right", "Split to the right", "Panes", '\u{F0574}'),
    ("split_down", "Split downwards", "Panes", '\u{F0573}'),
    ("close_pane", "Close pane", "Panes", '\u{F0156}'),
    ("next_pane", "Next pane", "Panes", '\u{F0142}'),
    ("previous_pane", "Previous pane", "Panes", '\u{F0141}'),
    ("focus_left", "Focus pane on the left", "Panes", '\u{F004D}'),
    ("focus_right", "Focus pane on the right", "Panes", '\u{F0054}'),
    ("focus_up", "Focus pane above", "Panes", '\u{F005D}'),
    ("focus_down", "Focus pane below", "Panes", '\u{F0045}'),
    ("resize_wider", "Make pane wider", "Panes", '\u{F0A9E}'),
    ("resize_narrower", "Make pane narrower", "Panes", '\u{F0A9E}'),
    ("resize_taller", "Make pane taller", "Panes", '\u{F0A9E}'),
    ("resize_shorter", "Make pane shorter", "Panes", '\u{F0A9E}'),
    ("move_pane_forward", "Move pane forward", "Panes", '\u{F0054}'),
    ("move_pane_backward", "Move pane backward", "Panes", '\u{F004D}'),
    ("toggle_broadcast", "Type into all panes (broadcast)", "Panes", '\u{F0003}'),
    ("next_layout", "Next layout", "Panes", '\u{F0573}'),
    ("toggle_zoom", "Zoom pane", "Panes", '\u{F0349}'),
    ("search", "Search scrollback", "Find", '\u{F0349}'),
    ("show_scrollback", "Open scrollback in pager", "Find", '\u{F0219}'),
    ("hints", "Pick a URL, path or hash (hints)", "Find", '\u{F0337}'),
    ("previous_prompt", "Jump to previous prompt", "Shell", '\u{F0143}'),
    ("next_prompt", "Jump to next prompt", "Shell", '\u{F0140}'),
    ("copy_last_command_output", "Copy last command's output", "Shell", '\u{F018F}'),
    ("ssh_connect", "Connect to SSH host…", "SSH", '\u{F08C0}'),
    ("toggle_files", "Remote files panel", "SSH", '\u{F024B}'),
    ("forward_port", "Forward a port…", "SSH", '\u{F04E1}'),
    ("copy", "Copy selection", "Clipboard", '\u{F018F}'),
    ("copy_cwd", "Copy current directory", "Clipboard", '\u{F024B}'),
    ("open_cwd", "Open current directory in file manager", "Shell", '\u{F0770}'),
    ("clear_scrollback", "Clear scrollback", "Scroll", '\u{F00E2}'),
    ("paste", "Paste", "Clipboard", '\u{F0192}'),
    ("scroll_home", "Scroll to top", "Scroll", '\u{F0603}'),
    ("scroll_end", "Scroll to bottom", "Scroll", '\u{F0601}'),
    ("increase_font_size", "Bigger text", "View", '\u{F0415}'),
    ("decrease_font_size", "Smaller text", "View", '\u{F0374}'),
    ("reset_font_size", "Reset text size", "View", '\u{F0450}'),
    ("settings", "Settings", "ionterm", '\u{F013}'),
    ("reload_config", "Reload config", "ionterm", '\u{F0450}'),
];

pub struct Command {
    pub action: Action,
    pub title: &'static str,
    pub group: &'static str,
    pub icon: char,
    /// Its keybinding, as shown ("Ctrl+Shift+T"); empty when unbound.
    pub keys: String,
}

/// The catalogue, with each action's keybinding from `bindings`.
pub fn catalogue(bindings: &HashMap<KeyCombo, Action>) -> Vec<Command> {
    COMMANDS
        .iter()
        .filter_map(|&(name, title, group, icon)| {
            let action = Action::parse(name)?;
            // The shortest combo, if several do the same thing.
            let keys = bindings
                .iter()
                .filter(|(_, a)| **a == action)
                .map(|(combo, _)| combo_label(combo))
                .min_by_key(|label| (label.len(), label.clone()))
                .unwrap_or_default();
            Some(Command {
                action,
                title,
                group,
                icon,
                keys,
            })
        })
        .collect()
}

/// Every action name in the catalogue.
pub fn names() -> impl Iterator<Item = &'static str> {
    COMMANDS.iter().map(|(name, ..)| *name)
}

/// An action's (name, title, group) in the catalogue, if it's there.
pub fn describe(action: Action) -> Option<(&'static str, &'static str, &'static str)> {
    COMMANDS
        .iter()
        .find(|(name, ..)| Action::parse(name) == Some(action))
        .map(|&(name, title, group, _)| (name, title, group))
}

/// A key combo as people write it: "Ctrl+Shift+T", "Alt+1", "Ctrl+,".
pub fn combo_label(combo: &KeyCombo) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (bit, name) in [(4, "Ctrl"), (2, "Alt"), (1, "Shift"), (8, "Super")] {
        if combo.mods & bit != 0 {
            parts.push(name.into());
        }
    }
    let key = match combo.key.as_str() {
        "comma" => ",".into(),
        "period" => ".".into(),
        "minus" => "-".into(),
        "equal" => "=".into(),
        "plus" => "+".into(),
        "left_bracket" => "[".into(),
        "right_bracket" => "]".into(),
        "slash" => "/".into(),
        "page_up" => "PageUp".into(),
        "page_down" => "PageDown".into(),
        key if key.len() == 1 => key.to_uppercase(),
        key => {
            let mut chars = key.chars();
            chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
        }
    };
    parts.push(key);
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_parses_and_shows_its_keys() {
        let bindings = crate::input::build_bindings(&HashMap::new());
        let commands = catalogue(&bindings);
        assert_eq!(commands.len(), COMMANDS.len(), "an action name in COMMANDS doesn't parse");
        let new_tab = commands.iter().find(|c| c.title == "New tab").unwrap();
        assert_eq!(new_tab.keys, "Ctrl+Shift+T");
        let settings = commands.iter().find(|c| c.title == "Settings").unwrap();
        assert_eq!(settings.keys, "Ctrl+,");
    }
}
