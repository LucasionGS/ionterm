//! The shortcuts cheatsheet on the settings page: every keybinding as
//! configured (the defaults with the user's own on top, plus the actions
//! that are left unbound), then the fixed keys and mouse gestures of the
//! terminal, the text fields and each panel.

use std::collections::HashMap;

use crate::commands::{self, combo_label};
use crate::input::{Action, KeyCombo};

/// A line of the cheatsheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Line {
    Heading(&'static str),
    Entry {
        /// "Ctrl+Shift+T / Ctrl+Tab"; empty when the action is unbound.
        keys: String,
        what: String,
        /// The action's name for `[keybindings]` in ionterm.toml; empty for
        /// the fixed keys.
        name: &'static str,
    },
}

/// Actions the command palette leaves out: (name, title, group).
const EXTRA_ACTIONS: &[(&str, &str, &str)] = &[
    ("paste_selection", "Paste the primary selection", "Clipboard"),
    ("scroll_line_up", "Scroll up a line", "Scroll"),
    ("scroll_line_down", "Scroll down a line", "Scroll"),
    ("scroll_page_up", "Scroll up a page", "Scroll"),
    ("scroll_page_down", "Scroll down a page", "Scroll"),
    ("command_palette", "Command palette", "ionterm"),
];

/// The order the keybinding groups are listed in.
const GROUPS: &[&str] = &["Tabs", "Panes", "Clipboard", "Scroll", "Find", "Shell", "SSH", "View", "ionterm"];

/// Keys and gestures that aren't keybindings: (heading, [(keys, what)]).
const FIXED: &[(&str, &[(&str, &str)])] = &[
    (
        "Mouse in the terminal",
        &[
            ("Drag", "Select text (copied to the primary selection)"),
            ("Double-click", "Select a word"),
            ("Triple-click", "Select a line"),
            ("Shift+Drag", "Select text in programs that use the mouse"),
            ("Middle-click", "Paste the primary selection"),
            ("Ctrl+Click", "Open the link or file under the pointer"),
            ("Ctrl+Shift+Right-click", "Select that command's output"),
            ("Click the command line", "Move the shell's cursor there"),
            ("Wheel", "Scroll (arrow keys in less, man and other full-screen programs)"),
            ("Drag a split border", "Resize the panes"),
            ("Drop files", "Paste their paths, or upload them in an SSH tab"),
        ],
    ),
    (
        "Tab bar",
        &[
            ("Double-click a tab", "Rename it"),
            ("Middle-click a tab", "Close it"),
            ("Right-click a tab", "Tab menu"),
            ("Drag a tab", "Move it"),
        ],
    ),
    (
        "Text fields",
        &[
            ("Left / Right", "Move the caret"),
            ("Ctrl+Left / Ctrl+Right", "Move by word"),
            ("Home / End", "Jump to the start or end"),
            ("Shift+(any of those)", "Select while moving"),
            ("Ctrl+A", "Select everything"),
            ("Backspace / Delete", "Delete a character, or the selection"),
            ("Ctrl+Backspace / Ctrl+Delete", "Delete a word"),
            ("Ctrl+C / Ctrl+X / Ctrl+V", "Copy, cut and paste (with or without shift)"),
            ("Click / Shift+Click", "Place the caret, or select up to there"),
        ],
    ),
    (
        "Scrollback search",
        &[
            ("Enter / Shift+Enter", "Previous (older) or next match"),
            ("Up / Down", "Previous or next match"),
            ("Alt+R", "Switch between plain text and regex"),
            ("Tab", "Back to the pane, keeping the highlights"),
            ("Escape", "Close the search"),
        ],
    ),
    (
        "Command palette and SSH picker",
        &[
            ("Up / Down", "Move through the list"),
            ("Tab / Shift+Tab", "Move through the list"),
            ("PageUp / PageDown", "Move by ten"),
            ("Enter", "Run the command, or connect"),
            ("Ctrl+Enter", "Connect in a split"),
            ("Ctrl+E", "Edit the selected host"),
            ("Escape", "Close"),
        ],
    ),
    (
        "SSH connection form",
        &[
            ("Tab / Shift+Tab", "Next or previous field"),
            ("Up / Down", "Next or previous field"),
            ("Left / Right", "Pick a colour (on the colour row)"),
            ("Enter", "Next field, or save on the last one"),
            ("Ctrl+Enter", "Save and connect"),
            ("Escape", "Cancel"),
        ],
    ),
    (
        "Hints",
        &[
            ("Type a label", "Open it (files open in your editor)"),
            ("Shift+Label", "Copy it instead"),
            ("Alt+Label", "Type it into the command line"),
            ("Backspace", "Take back a letter"),
            ("Enter", "Pick the last label left"),
            ("Escape", "Cancel"),
        ],
    ),
    (
        "Remote files panel",
        &[
            ("Up / Down / PageUp / PageDown", "Move through the list"),
            ("Home / End", "First or last entry"),
            ("Enter / Double-click", "Open the file or folder"),
            ("Right", "Open the folder"),
            ("Left / Backspace", "Up to the parent folder"),
            ("Type", "Filter the list"),
            ("Ctrl+L", "Clear the filter"),
            ("Escape", "Clear the filter, or back to the terminal"),
            ("Delete", "Delete"),
            ("F2", "Rename"),
            ("F5 / Ctrl+R", "Refresh"),
            ("Ctrl+N", "New folder"),
            ("Ctrl+U", "Upload files"),
            ("Ctrl+D", "Download"),
            ("Ctrl+H", "Show or hide hidden files"),
            ("Ctrl+C", "Copy the path"),
            ("Right-click", "Menu for that entry"),
        ],
    ),
    (
        "Dialogs",
        &[
            ("Enter", "The highlighted button"),
            ("Left / Right / Tab", "Highlight another button"),
            ("Letter in brackets", "Press that button"),
            ("A", "Tick \"Do this for all\""),
            ("Escape", "Dismiss"),
        ],
    ),
    (
        "Settings",
        &[
            ("Up / Down", "Move between settings"),
            ("Left / Right", "Change the value"),
            ("Enter / Space", "Switch, open or start typing"),
            ("Delete", "Back to the default"),
            ("Tab", "Next section"),
            ("PageUp / PageDown", "Previous or next section"),
        ],
    ),
    (
        "Other",
        &[
            ("Enter", "Reconnect a dropped SSH connection"),
            ("Escape / Ctrl+C", "Cancel a ZMODEM transfer"),
        ],
    ),
];

/// Title and group for an action that isn't `goto_tab`.
fn describe(action: Action) -> Option<(&'static str, &'static str, &'static str)> {
    commands::describe(action).or_else(|| {
        EXTRA_ACTIONS
            .iter()
            .find(|(name, _, _)| Action::parse(name) == Some(action))
            .copied()
    })
}

/// Combos joined for display, shortest first.
fn join(combos: &[&KeyCombo]) -> String {
    let mut labels: Vec<String> = combos.iter().map(|c| combo_label(c)).collect();
    labels.sort_by_key(|l| (l.len(), l.clone()));
    labels.dedup();
    labels.join(" / ")
}

/// The keybinding entries, grouped like the command palette.
fn bound(bindings: &HashMap<KeyCombo, Action>) -> Vec<(&'static str, Line)> {
    let mut by_action: HashMap<Action, Vec<&KeyCombo>> = HashMap::new();
    for (combo, action) in bindings {
        by_action.entry(*action).or_default().push(combo);
    }
    let mut out = Vec::new();

    // Every described action, bound or not, so the unbound ones can be found.
    let names = commands::names().chain(EXTRA_ACTIONS.iter().map(|(name, _, _)| *name));
    for name in names {
        let Some(action) = Action::parse(name) else { continue };
        let Some((name, title, group)) = describe(action) else { continue };
        let keys = by_action.get(&action).map(|c| join(c)).unwrap_or_default();
        out.push((
            group,
            Line::Entry {
                keys,
                what: title.to_string(),
                name,
            },
        ));
    }

    // goto_tab 1–9 as one line when they follow the usual pattern.
    let mut tabs: Vec<(usize, &KeyCombo)> = by_action
        .iter()
        .filter_map(|(action, combos)| match action {
            Action::GotoTab(n) => Some(combos.iter().map(move |c| (*n, *c))),
            _ => None,
        })
        .flatten()
        .collect();
    tabs.sort_by_key(|(n, c)| (*n, combo_label(c)));
    let pattern = tabs.len() > 1
        && tabs.iter().enumerate().all(|(i, (n, c))| *n == i + 1 && c.key == n.to_string() && c.mods == tabs[0].1.mods);
    if pattern {
        let (first, last) = (combo_label(tabs[0].1), tabs.len());
        out.push(("Tabs", Line::Entry {
            keys: format!("{first} … {last}"),
            what: format!("Go to tab 1–{last}"),
            name: "goto_tab",
        }));
    } else {
        for (n, combo) in tabs {
            out.push(("Tabs", Line::Entry {
                keys: combo_label(combo),
                what: format!("Go to tab {n}"),
                name: "goto_tab",
            }));
        }
    }
    out
}

/// The cheatsheet, keeping the entries that match `filter` (in their keys,
/// description or action name, ignoring case) and the headings over them.
pub fn lines(bindings: &HashMap<KeyCombo, Action>, filter: &str) -> Vec<Line> {
    let filter = filter.trim().to_lowercase();
    let matches = |line: &Line| match line {
        Line::Entry { keys, what, name } => {
            filter.is_empty()
                || keys.to_lowercase().contains(&filter)
                || what.to_lowercase().contains(&filter)
                || name.contains(filter.as_str())
        }
        Line::Heading(_) => false,
    };
    let mut groups: Vec<(&'static str, Vec<Line>)> = GROUPS.iter().map(|g| (*g, Vec::new())).collect();
    for (group, line) in bound(bindings) {
        match groups.iter_mut().find(|(g, _)| *g == group) {
            Some((_, lines)) => lines.push(line),
            None => groups.push((group, vec![line])),
        }
    }
    // Bound keys first, then what's left to bind.
    for (_, lines) in &mut groups {
        lines.sort_by_key(|line| matches!(line, Line::Entry { keys, .. } if keys.is_empty()));
    }
    let fixed = FIXED.iter().map(|(heading, entries)| {
        let lines = entries
            .iter()
            .map(|(keys, what)| Line::Entry {
                keys: keys.to_string(),
                what: what.to_string(),
                name: "",
            })
            .collect();
        (*heading, lines)
    });

    let mut out = Vec::new();
    for (heading, lines) in groups.into_iter().chain(fixed) {
        let shown: Vec<Line> = lines.into_iter().filter(|l| matches(l)).collect();
        if !shown.is_empty() {
            out.push(Line::Heading(heading));
            out.extend(shown);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'a>(lines: &'a [Line], what: &str) -> Option<&'a str> {
        lines.iter().find_map(|line| match line {
            Line::Entry { keys, what: w, .. } if w == what => Some(keys.as_str()),
            _ => None,
        })
    }

    #[test]
    fn lists_bindings_with_overrides() {
        let mut user = HashMap::new();
        user.insert("ctrl+alt+n".to_string(), "new_tab".to_string());
        user.insert("ctrl+shift+b".to_string(), "none".to_string());
        let lines = lines(&crate::input::build_bindings(&user), "");
        assert_eq!(entry(&lines, "New tab"), Some("Ctrl+Alt+N / Ctrl+Shift+T"));
        assert_eq!(entry(&lines, "Next tab"), Some("Ctrl+Tab / Ctrl+Shift+Right"));
        assert_eq!(entry(&lines, "Go to tab 1–9"), Some("Alt+1 … 9"));
        // Unbound actions are still listed, without keys.
        assert_eq!(entry(&lines, "Type into all panes (broadcast)"), Some(""));
        assert_eq!(entry(&lines, "Split to the right"), Some(""));
        assert!(lines.contains(&Line::Heading("Text fields")));
    }

    #[test]
    fn filters_and_keeps_headings() {
        let lines = lines(&crate::input::build_bindings(&HashMap::new()), "regex");
        assert_eq!(lines, [
            Line::Heading("Scrollback search"),
            Line::Entry {
                keys: "Alt+R".into(),
                what: "Switch between plain text and regex".into(),
                name: "",
            },
        ]);
    }
}
