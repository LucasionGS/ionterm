//! What shell integration gives the window: jumping between prompts,
//! copying or selecting a command's output, marks beside failed commands,
//! and tab progress bars (from OSC 9;4 or a percentage in the output).

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ionterm_render::ChromeItem;
use ionterm_vt::{Progress, ProgressState};

use super::{App, TermWindow};
use crate::pane::{Pane, PaneProgress, SelectionKind, SelectionState};
use crate::tabbar::TabProgress;
use crate::theme::Theme;

/// A percentage read from the output lapses after this long unchanged.
const GUESSED_PROGRESS_TIME: Duration = Duration::from_secs(4);
/// Seconds for the stripe of an indeterminate progress bar to cross a tab.
const STRIPE_PERIOD: f32 = 1.4;

/// Where an indeterminate progress bar's stripe is now, 0 to 1.
pub(super) fn progress_phase() -> f32 {
    static START: OnceLock<Instant> = OnceLock::new();
    let elapsed = START.get_or_init(Instant::now).elapsed().as_secs_f32();
    (elapsed / STRIPE_PERIOD).fract()
}

fn from_reported(progress: Progress) -> TabProgress {
    TabProgress {
        fraction: progress.percent.map(|p| f32::from(p) / 100.0),
        state: progress.state,
    }
}

impl Pane {
    /// Applies a program's reported progress; `None` clears it.
    pub(super) fn report_progress(&mut self, progress: Option<Progress>) -> bool {
        let before = self.progress.map(|p| p.shown);
        self.progress = progress.map(|p| PaneProgress {
            shown: from_reported(p),
            guessed: false,
            updated: Instant::now(),
        });
        before != self.progress.map(|p| p.shown)
    }

    /// Reads a percentage off the line being rewritten, unless the program
    /// reports its progress itself. Returns whether the bar changed.
    pub(super) fn guess_progress(&mut self) -> bool {
        if self.progress.is_some_and(|p| !p.guessed) {
            return false;
        }
        let Some(percent) = self.lock().progress_hint() else { return false };
        let shown = TabProgress {
            fraction: Some(percent / 100.0),
            state: ProgressState::Normal,
        };
        let changed = self.progress.map(|p| p.shown) != Some(shown);
        self.progress = Some(PaneProgress {
            shown,
            guessed: true,
            updated: Instant::now(),
        });
        changed
    }
}

impl TermWindow {
    /// Drops guessed progress that stopped changing. Returns when to check
    /// again.
    pub(super) fn expire_progress(&mut self, now: Instant) -> Option<Instant> {
        let mut next = None;
        let mut changed = false;
        for pane in self.tabs.iter_mut().flat_map(|t| &mut t.panes) {
            let Some(progress) = pane.progress.filter(|p| p.guessed) else { continue };
            let lapses = progress.updated + GUESSED_PROGRESS_TIME;
            if now >= lapses {
                pane.progress = None;
                changed = true;
            } else {
                next = [next, Some(lapses)].into_iter().flatten().min();
            }
        }
        if changed {
            self.window.request_redraw();
        }
        next
    }

    /// A tab's progress: its active pane's, or else any of its panes'.
    pub(super) fn tab_progress(&self, tab: usize) -> Option<TabProgress> {
        let tab = &self.tabs[tab];
        tab.active_pane()
            .progress
            .or_else(|| tab.panes.iter().find_map(|p| p.progress))
            .map(|p| p.shown)
    }

    /// A red bar in the left padding beside prompts whose command failed.
    pub(super) fn draw_failed_marks(&self, theme: &Theme, out: &mut Vec<ChromeItem>) {
        let tab = &self.tabs[self.active_tab];
        let m = self.renderer.cell_metrics();
        let scale = self.scale_factor as f32;
        let red = theme.palette().colors[1];
        for pane in tab.panes.iter().filter(|p| tab.is_visible(p.id)) {
            let failed = pane.lock().failed_commands_in_view();
            if failed.is_empty() {
                continue;
            }
            let padding = pane.origin[0] - pane.area.x;
            let width = (3.0 * scale).round().clamp(1.0, padding.max(1.0));
            // Centred in the padding, or on the grid's edge without any.
            let x = if padding >= width { pane.area.x + ((padding - width) / 2.0).round() } else { pane.origin[0] };
            for (row, _status) in failed {
                let y = pane.origin[1] + row as f32 * m.height;
                out.push(ChromeItem::Rect {
                    rect: [x, y + (2.0 * scale).round(), width, m.height - (4.0 * scale).round()],
                    color: red,
                    alpha: 1.0,
                });
            }
        }
    }
}

impl App {
    /// Scrolls the active pane to the previous or next prompt.
    pub(super) fn jump_to_prompt(&mut self, forward: bool) {
        let Some(w) = &mut self.window else { return };
        let pane = w.active_pane();
        let mut term = pane.lock();
        let prompts = term.prompt_lines();
        if prompts.is_empty() {
            drop(term);
            w.show_toast("No prompt marks here: this shell isn't set up for shell integration");
            return;
        }
        let top = term.viewport_to_absolute(0);
        let target = if forward {
            prompts.iter().copied().find(|&p| p > top)
        } else {
            prompts.iter().copied().rev().find(|&p| p < top)
        };
        match target {
            Some(line) => term.scroll_to_line(line),
            None if forward => term.scroll_to_bottom(),
            None => {}
        }
        drop(term);
        w.window.request_redraw();
    }

    /// Copies the last command's output to the clipboard.
    pub(super) fn copy_last_output(&mut self) {
        let Some(w) = &mut self.window else { return };
        let text = {
            let pane = w.active_pane();
            let term = pane.lock();
            term.last_command_output().map(|(start, end)| {
                let text = term.text_between((start, 0), (end, pane.grid.0.saturating_sub(1)));
                (text, end - start + 1)
            })
        };
        match text {
            Some((text, lines)) => {
                let plural = if lines == 1 { "" } else { "s" };
                w.show_toast(format!("Copied the last command's output ({lines} line{plural})"));
                self.set_clipboard(text, false);
            }
            None => w.show_toast("No command output to copy"),
        }
    }
}

/// Selects the output of the command around viewport `row`. Returns whether
/// there was any.
pub(super) fn select_command_output(pane: &mut Pane, row: usize) -> bool {
    let range = {
        let term = pane.lock();
        term.command_output_at(term.viewport_to_absolute(row))
    };
    let Some((start, end)) = range else { return false };
    pane.selection = Some(SelectionState {
        anchor: (start, 0),
        head: (end, pane.grid.0.saturating_sub(1)),
        kind: SelectionKind::Simple,
    });
    true
}
