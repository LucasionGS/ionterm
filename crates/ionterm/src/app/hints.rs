//! Hints mode in the window (ctrl+shift+e): labels over what can be picked
//! in the active pane, typed to open it, with shift to copy it instead and
//! alt to type it into the command line. Finding and labelling is in
//! `crate::hints`.

use std::path::PathBuf;

use ionterm_render::ChromeItem;
use ionterm_vt::{Flags, Term};
use winit::event::KeyEvent;
use winit::keyboard::{Key, NamedKey};

use super::{App, TermWindow};
use crate::files;
use crate::hints::{self, HintKind, OpenFile};
use crate::input;
use crate::layout::PaneId;
use crate::links;
use crate::pane::Pane;
use crate::tabbar::mix;
use crate::theme::Theme;

/// What can be picked, while hints are shown.
pub struct HintsMode {
    pane: PaneId,
    targets: Vec<Target>,
    /// Label letters typed so far.
    typed: String,
    /// A capital letter was typed: copy rather than open.
    copy: bool,
    /// Alt was held: type it into the command line rather than open.
    insert: bool,
}

struct Target {
    label: String,
    kind: HintKind,
    /// What's acted on: the URL (an OSC 8 link's target), or the text.
    text: String,
    /// The text on screen, as (viewport row, column, characters) runs.
    runs: Vec<(usize, usize, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pick {
    Open,
    Copy,
    Insert,
}

/// Everything pickable in the visible part of a pane, most recent first.
fn find_targets(term: &Term, rows: usize, exists: Option<&dyn Fn(&str) -> bool>) -> Vec<Target> {
    let mut targets = Vec::new();
    let mut row = 0;
    while row < rows {
        // A logical line: rows joined where they soft-wrapped.
        let mut chars: Vec<(char, usize, usize, u32)> = Vec::new();
        while let Some(line) = term.line(term.viewport_to_absolute(row)) {
            for (col, cell) in line.cells.iter().enumerate() {
                if !cell.flags.contains(Flags::WIDE_SPACER) {
                    chars.push((cell.c, row, col, cell.hyperlink));
                }
            }
            row += 1;
            if !line.wrapped || row >= rows {
                break;
            }
        }
        let text: Vec<char> = chars.iter().map(|c| c.0).collect();
        let mut taken = vec![false; chars.len()];
        // OSC 8 links first: what they point at is what counts.
        let mut i = 0;
        while i < chars.len() {
            let id = chars[i].3;
            if id == 0 {
                i += 1;
                continue;
            }
            let start = i;
            while i < chars.len() && chars[i].3 == id {
                i += 1;
            }
            if let Some(uri) = term.hyperlink(id) {
                taken[start..i].fill(true);
                targets.push(target(&chars[start..i], HintKind::Url, uri.to_string()));
            }
        }
        for found in hints::scan_line(&text, exists) {
            if taken[found.start..found.end].iter().any(|&t| t) {
                continue;
            }
            let shown: String = text[found.start..found.end].iter().collect();
            targets.push(target(&chars[found.start..found.end], found.kind, shown));
        }
    }
    // The newest output (lowest on screen) gets the first labels.
    targets.sort_by_key(|t| std::cmp::Reverse(t.runs.first().map(|r| (r.0, r.1))));
    let labels = hints::labels(targets.len());
    for (target, label) in targets.iter_mut().zip(labels) {
        target.label = label;
    }
    targets
}

fn target(chars: &[(char, usize, usize, u32)], kind: HintKind, text: String) -> Target {
    let mut runs: Vec<(usize, usize, String)> = Vec::new();
    for &(c, row, col, _) in chars {
        match runs.last_mut() {
            Some(run) if run.0 == row => run.2.push(c),
            _ => runs.push((row, col, c.to_string())),
        }
    }
    Target {
        label: String::new(),
        kind,
        text,
        runs,
    }
}

/// Paths in a pane are only checkable (and openable) when it's a local
/// shell: in an SSH pane they're on the server.
fn is_remote(pane: &Pane) -> bool {
    pane.connection.is_some() || pane.detected_ssh.is_some()
}

impl TermWindow {
    /// Draws the labels over the active pane, dimming everything else.
    pub(super) fn draw_hints(&self, theme: &Theme, out: &mut Vec<ChromeItem>) {
        let Some(mode) = &self.hints else { return };
        let Some(pane) = self.tabs[self.active_tab].pane(mode.pane) else { return };
        let m = self.renderer.cell_metrics();
        let scale = self.scale_factor as f32;
        let bg = theme.background.0;
        let fg = theme.foreground.0;
        let accent = theme.cursor.0;
        let area = pane.area;
        out.push(ChromeItem::Rect {
            rect: [area.x, area.y, area.width, area.height],
            color: bg,
            alpha: 0.6,
        });
        let at = |row: usize, col: usize| (pane.origin[0] + col as f32 * m.width, pane.origin[1] + row as f32 * m.height);
        for target in mode.targets.iter().filter(|t| t.label.starts_with(&mode.typed)) {
            for (row, col, text) in &target.runs {
                let (x, y) = at(*row, *col);
                out.push(ChromeItem::Text {
                    text: text.clone(),
                    x,
                    y,
                    color: fg,
                    bold: false,
                });
                let width = ionterm_render::text_columns(text) as f32 * m.width;
                out.push(ChromeItem::Rect {
                    rect: [x, y + m.height - (1.0 * scale).round().max(1.0), width, (1.0 * scale).round().max(1.0)],
                    color: accent,
                    alpha: 1.0,
                });
            }
            // The label covers the start of the text, like kitty's.
            let Some((row, col, _)) = target.runs.first() else { continue };
            let (x, y) = at(*row, *col);
            let width = target.label.chars().count() as f32 * m.width;
            out.push(ChromeItem::RoundedRect {
                rect: [x - (1.0 * scale).round(), y, width + (2.0 * scale).round(), m.height],
                radius: (3.0 * scale).round(),
                top_only: false,
                color: accent,
                alpha: 1.0,
            });
            let typed = mode.typed.chars().count();
            let (done, rest): (String, String) = (target.label.chars().take(typed).collect(), target.label.chars().skip(typed).collect());
            if !done.is_empty() {
                out.push(ChromeItem::Text {
                    text: done,
                    x,
                    y,
                    color: mix(bg, accent, 0.45),
                    bold: true,
                });
            }
            out.push(ChromeItem::Text {
                text: rest,
                x: x + typed as f32 * m.width,
                y,
                color: bg,
                bold: true,
            });
        }
    }
}

impl App {
    /// Shows hints over the active pane, or hides them.
    pub(super) fn toggle_hints(&mut self) {
        let Some(w) = &mut self.window else { return };
        if w.hints.take().is_some() {
            w.window.request_redraw();
            return;
        }
        let pane = w.active_pane();
        let cwd = pane.working_dir();
        let local = !is_remote(pane);
        let exists = |path: &str| hints::resolve(path, cwd.as_deref()).exists();
        let check: Option<&dyn Fn(&str) -> bool> = if local { Some(&exists) } else { None };
        let targets = find_targets(&pane.lock(), pane.grid.1, check);
        if targets.is_empty() {
            w.show_toast("Nothing to pick here");
            return;
        }
        w.hints = Some(HintsMode {
            pane: pane.id,
            targets,
            typed: String::new(),
            copy: false,
            insert: false,
        });
        w.window.request_redraw();
    }

    /// A key while hints are shown: label letters, Backspace, Enter, Escape.
    pub(super) fn hints_key(&mut self, event: &KeyEvent) {
        let Some(w) = &mut self.window else { return };
        let alt = w.modifiers.alt_key();
        let Some(mode) = &mut w.hints else { return };
        w.window.request_redraw();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                w.hints = None;
                return;
            }
            Key::Named(NamedKey::Backspace) => {
                mode.typed.pop();
                return;
            }
            Key::Named(NamedKey::Enter) => {}
            _ => {
                let Some(c) = event.text.as_deref().and_then(|t| t.chars().next()).or_else(|| match &event.logical_key {
                    Key::Character(s) => s.chars().next(),
                    _ => None,
                }) else {
                    return;
                };
                if !c.is_alphabetic() {
                    return;
                }
                let next = format!("{}{}", mode.typed, c.to_lowercase());
                // Letters that no label continues with are ignored.
                if !mode.targets.iter().any(|t| t.label.starts_with(&next)) {
                    return;
                }
                mode.typed = next;
                mode.copy |= c.is_uppercase();
                mode.insert |= alt;
            }
        }
        // Done once one label is left and fully typed (or Enter picks it).
        let mut left = mode.targets.iter().filter(|t| t.label.starts_with(&mode.typed));
        let (Some(only), None) = (left.next(), left.next()) else { return };
        if only.label != mode.typed && event.logical_key != Key::Named(NamedKey::Enter) {
            return;
        }
        let pick = if mode.insert {
            Pick::Insert
        } else if mode.copy {
            Pick::Copy
        } else {
            Pick::Open
        };
        let pane = mode.pane;
        let (kind, text) = (only.kind.clone(), only.text.clone());
        w.hints = None;
        self.pick_hint(pane, kind, text, pick);
    }

    fn pick_hint(&mut self, pane_id: PaneId, kind: HintKind, text: String, pick: Pick) {
        let template = self.config.hints.open_file.clone();
        let Some(w) = &mut self.window else { return };
        let Some(pane) = w.tabs[w.active_tab].pane(pane_id) else { return };
        let remote = is_remote(pane);
        let cwd = pane.working_dir();
        // Hashes and addresses have nothing to open; remote paths aren't here.
        let pick = match (&kind, pick) {
            (HintKind::Hash | HintKind::Ip, Pick::Open) => Pick::Copy,
            (HintKind::Path | HintKind::FileLine { .. }, Pick::Open) if remote => Pick::Copy,
            _ => pick,
        };
        match pick {
            Pick::Copy => {
                let note = if remote && matches!(kind, HintKind::Path | HintKind::FileLine { .. }) { " (it's on the server)" } else { "" };
                w.show_toast(format!("Copied {text}{note}"));
                self.set_clipboard(text, false);
            }
            Pick::Insert => {
                let mode = pane.lock().mode();
                pane.write(&input::encode_paste(&text, mode));
            }
            Pick::Open => match kind {
                HintKind::Url => links::open(&text),
                HintKind::Path => {
                    let path = hints::resolve(&text, cwd.as_deref());
                    self.open_file(path, None, None, cwd, &template);
                }
                HintKind::FileLine { path, line, col } => {
                    let path = hints::resolve(&path, cwd.as_deref());
                    self.open_file(path, Some(line), col, cwd, &template);
                }
                HintKind::Hash | HintKind::Ip => {}
            },
        }
    }

    /// Opens a local file in the editor (or the configured program), and a
    /// folder in the file manager.
    fn open_file(&mut self, path: PathBuf, line: Option<u32>, col: Option<u32>, cwd: Option<PathBuf>, template: &str) {
        let Some(w) = &mut self.window else { return };
        if !path.exists() {
            w.show_toast(format!("Not found: {}", path.display()));
            return;
        }
        if path.is_dir() {
            files::reveal(&path);
            return;
        }
        match hints::open_file_command(template, &path, line, col) {
            Some(OpenFile::Tab(argv)) => self.open_command_tab(argv, cwd),
            Some(OpenFile::Detached(argv)) => {
                let started = std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                if let Err(error) = started {
                    w.show_toast(format!("Couldn't run {}: {error}", argv[0]));
                }
            }
            // No editor anywhere: show it in the file manager.
            None => files::reveal(&path),
        }
    }
}
