//! Command blocks: a prompt, the command run from it and its output, from
//! the shell integration's marks (OSC 133). This is the part that needs no
//! window: finding blocks, what the sticky header says, the block menu
//! widget and the scrollbar's geometry.
//!
//! None of it changes how the terminal behaves: the header, the scrollbar
//! and the block handle only show over padding or the top row, and only
//! when there's something to show.

use std::time::{Duration, SystemTime};

use ionterm_render::{CellMetrics, ChromeItem, text_columns};
use ionterm_vt::{Rgb, Term, TermMode};

use crate::layout::Rect;
use crate::tabbar::{mix, truncate};
use crate::theme::Theme;

/// One command: where its prompt is, what ran, what it wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The prompt's absolute line.
    pub prompt: usize,
    /// The command line, when the shell integration knows it.
    pub command: Option<String>,
    /// Its output's first and last absolute line.
    pub output: Option<(usize, usize)>,
    /// Its exit status, once it finished.
    pub status: Option<i32>,
    /// The last line that belongs to it (before the next prompt).
    pub end: usize,
    /// A later prompt exists: it isn't the command line being typed or the
    /// command still running.
    pub finished: bool,
}

/// The block an absolute line belongs to.
pub fn block_at(term: &Term, absolute: usize) -> Option<Block> {
    let prompts = term.prompt_lines();
    let index = prompts.iter().rposition(|&p| p <= absolute)?;
    let prompt = prompts[index];
    let next = prompts.get(index + 1).copied();
    let end = match next {
        Some(next) => next - 1,
        None => term.viewport_to_absolute(term.rows().saturating_sub(1)).max(prompt),
    };
    let row = term.line(prompt)?;
    Some(Block {
        prompt,
        command: term.command_of(prompt).map(str::to_string),
        output: term.command_output_at(prompt),
        status: row.status,
        end,
        finished: next.is_some(),
    })
}

/// The block keyboard actions work on: the one at the top of the view when
/// scrolled back, else the last one that ran (finished, or running).
pub fn current_block(term: &Term, running: bool) -> Option<Block> {
    if term.display_offset() > 0 {
        return block_at(term, term.viewport_to_absolute(0));
    }
    let prompts = term.prompt_lines();
    // At the prompt, the last prompt is the command line being typed.
    let skip = usize::from(!running);
    let prompt = *prompts.iter().rev().nth(skip)?;
    block_at(term, prompt)
}

/// The command and its output as a Markdown code block.
pub fn markdown(command: Option<&str>, output: &str) -> String {
    let fence = if output.contains("```") { "````" } else { "```" };
    match command {
        Some(command) => format!("{fence}console\n$ {command}\n{output}\n{fence}\n"),
        None => format!("{fence}\n{output}\n{fence}\n"),
    }
}

/// A short, readable duration: `850ms`, `2.5s`, `1m 03s`, `2h 05m`.
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0 => format!("{}ms", d.as_millis()),
        1..=59 => format!("{:.1}s", d.as_secs_f32()),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// How long ago something happened: `just now`, `5m ago`, `2h ago`.
pub fn ago(when: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(when).map_or(0, |d| d.as_secs());
    match secs {
        0..=9 => "just now".into(),
        10..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

//#region Sticky header

/// What the sticky header shows: the command whose output fills the top
/// of the view while its prompt is scrolled out of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sticky {
    pub prompt: usize,
    pub text: String,
    pub status: Option<i32>,
    /// `took 2.5s · 5m ago`, or `running 1m 03s`.
    pub detail: String,
}

/// The sticky header for a pane: only on the main screen, when its top
/// row is a command's output and that command's prompt is above the view,
/// and only while scrolled back or while that command still runs (at the
/// prompt, the view simply shows the latest output).
pub fn sticky(term: &Term, running: Option<Duration>) -> Option<Sticky> {
    if term.mode().contains(TermMode::ALT_SCREEN) {
        return None;
    }
    let top = term.viewport_to_absolute(0);
    let block = block_at(term, top)?;
    let (start, _) = block.output?;
    if block.prompt >= top || top < start || top > block.end {
        return None;
    }
    let scrolled = term.display_offset() > 0;
    let live = running.filter(|_| !block.finished);
    if !scrolled && live.is_none() {
        return None;
    }
    let text = block.command.clone().unwrap_or_else(|| {
        // No command line from the shell: the prompt row as it reads.
        let line = term.text_between((block.prompt, 0), (block.prompt, term.cols() - 1));
        line.trim().to_string()
    });
    let row = term.line(block.prompt)?;
    let detail = match (live, row.ran) {
        (Some(running), _) => format!("running {}", duration(running)),
        (None, Some(ran)) => format!("took {} · {}", duration(ran.duration), ago(ran.finished)),
        (None, None) => String::new(),
    };
    Some(Sticky {
        prompt: block.prompt,
        text,
        status: block.status.filter(|_| live.is_none()),
        detail,
    })
}

/// Draws the header over a pane's top row.
#[allow(clippy::too_many_arguments)]
pub fn draw_sticky(sticky: &Sticky, area: Rect, origin: [f32; 2], cols: usize, hovered: bool, theme: &Theme, m: CellMetrics, scale: f32, out: &mut Vec<ChromeItem>) {
    let bg = theme.background.0;
    let fg = theme.foreground.0;
    let accent = theme.cursor.0;
    let y = origin[1];
    let height = m.height;
    let back = mix(bg, fg, if hovered { 0.10 } else { 0.06 });
    out.push(ChromeItem::Rect {
        rect: [area.x, area.y, area.width, y + height - area.y],
        color: back,
        alpha: 1.0,
    });
    out.push(ChromeItem::Rect {
        rect: [area.x, y + height, area.width, scale.round().max(1.0)],
        color: accent,
        alpha: 0.45,
    });
    let (mark, mark_color) = match sticky.status {
        Some(0) => ("✓", theme.palette.colors.get(2).map_or(accent, |c| c.0)),
        Some(_) => ("✗", theme.palette.colors.get(1).map_or(accent, |c| c.0)),
        None => ("❯", accent),
    };
    out.push(ChromeItem::Text {
        text: mark.into(),
        x: origin[0],
        y,
        color: mark_color,
        bold: true,
    });
    let detail_cols = text_columns(&sticky.detail);
    let room = cols.saturating_sub(detail_cols + 5);
    out.push(ChromeItem::Text {
        text: truncate(&sticky.text, room),
        x: origin[0] + 2.0 * m.width,
        y,
        color: fg,
        bold: true,
    });
    if !sticky.detail.is_empty() {
        out.push(ChromeItem::Text {
            text: sticky.detail.clone(),
            x: origin[0] + (cols - detail_cols) as f32 * m.width,
            y,
            color: mix(fg, bg, 0.4),
            bold: false,
        });
    }
}

//#endregion

//#region Scrollbar

/// Where a pane's scrollbar goes: a thin strip in its right padding.
pub fn scrollbar_track(area: Rect, scale: f32) -> Rect {
    let width = (6.0 * scale).round();
    let inset = (2.0 * scale).round();
    Rect::new(area.x + area.width - width - inset, area.y + inset, width, (area.height - 2.0 * inset).max(1.0))
}

/// The part of the track showing the view: (y, height).
pub fn scrollbar_thumb(track: Rect, total: usize, top: usize, rows: usize, scale: f32) -> (f32, f32) {
    let total = total.max(1) as f32;
    let height = (track.height * rows as f32 / total).max((16.0 * scale).round()).min(track.height);
    let y = track.y + (track.height - height) * (top as f32 / (total - rows as f32).max(1.0)).clamp(0.0, 1.0);
    (y, height)
}

/// The shown line a point on the track stands for, with the view centred
/// on it.
pub fn scrollbar_line(track: Rect, y: f32, total: usize, rows: usize) -> usize {
    let fraction = ((y - track.y) / track.height).clamp(0.0, 1.0);
    let line = (fraction * total as f32) as usize;
    line.saturating_sub(rows / 2)
}

/// A mark on the scrollbar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    Prompt,
    Failed,
    Match,
}

#[allow(clippy::too_many_arguments)]
pub fn draw_scrollbar(track: Rect, thumb: (f32, f32), markers: &[(usize, Marker)], total: usize, hovered: bool, theme: &Theme, scale: f32, out: &mut Vec<ChromeItem>) {
    let fg = theme.foreground.0;
    let radius = track.width / 2.0;
    out.push(ChromeItem::RoundedRect {
        rect: [track.x, track.y, track.width, track.height],
        radius,
        top_only: false,
        color: fg,
        alpha: if hovered { 0.10 } else { 0.05 },
    });
    out.push(ChromeItem::RoundedRect {
        rect: [track.x, thumb.0, track.width, thumb.1],
        radius,
        top_only: false,
        color: fg,
        alpha: if hovered { 0.40 } else { 0.28 },
    });
    let tick = (2.0 * scale).round().max(1.0);
    let red = theme.palette.colors.get(1).map_or(fg, |c| c.0);
    let accent = theme.cursor.0;
    for &(line, marker) in markers {
        let y = track.y + track.height * line as f32 / total.max(1) as f32;
        let (color, alpha, inset) = match marker {
            Marker::Prompt => (fg, 0.35, track.width * 0.25),
            Marker::Failed => (red, 1.0, 0.0),
            Marker::Match => (accent, 0.9, 0.0),
        };
        out.push(ChromeItem::Rect {
            rect: [track.x + inset, y.min(track.y + track.height - tick), track.width - 2.0 * inset, tick],
            color,
            alpha,
        });
    }
}

//#endregion

//#region Block menu

/// What the block menu offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockAction {
    CopyCommand,
    CopyOutput,
    CopyMarkdown,
    Rerun,
    OpenInEditor,
    OpenInPager,
    SaveOutput,
    Fold,
}

impl BlockAction {
    fn label(self, folded: bool) -> &'static str {
        match self {
            Self::CopyCommand => "Copy command",
            Self::CopyOutput => "Copy output",
            Self::CopyMarkdown => "Copy both as Markdown",
            Self::Rerun => "Run again",
            Self::OpenInEditor => "Open output in editor",
            Self::OpenInPager => "Open output in pager",
            Self::SaveOutput => "Save output to Downloads",
            Self::Fold if folded => "Unfold output",
            Self::Fold => "Fold output",
        }
    }

    /// Whether it makes sense for a block.
    fn applies(self, block: &Block) -> bool {
        match self {
            Self::CopyCommand | Self::Rerun => block.command.is_some(),
            Self::CopyMarkdown => block.output.is_some() || block.command.is_some(),
            Self::Fold => block.finished && block.output.is_some_and(|(s, e)| e - s + 1 > 7),
            _ => block.output.is_some(),
        }
    }
}

const ACTIONS: [BlockAction; 8] = [
    BlockAction::CopyCommand,
    BlockAction::CopyOutput,
    BlockAction::CopyMarkdown,
    BlockAction::Rerun,
    BlockAction::OpenInEditor,
    BlockAction::OpenInPager,
    BlockAction::SaveOutput,
    BlockAction::Fold,
];

/// The menu of things to do with one block, opened from its handle.
pub struct BlockMenu {
    pub pane: crate::layout::PaneId,
    pub block: Block,
    pub items: Vec<BlockAction>,
    pub folded: bool,
    /// Top-left corner, where it was opened.
    pub x: f32,
    pub y: f32,
    pub hover: Option<usize>,
}

impl BlockMenu {
    pub fn new(pane: crate::layout::PaneId, block: Block, folded: bool, x: f32, y: f32) -> Option<Self> {
        let items: Vec<BlockAction> = ACTIONS.into_iter().filter(|a| a.applies(&block)).collect();
        (!items.is_empty()).then_some(Self {
            pane,
            block,
            items,
            folded,
            x,
            y,
            hover: None,
        })
    }

    /// Moves the keyboard highlight.
    pub fn step(&mut self, delta: isize) {
        let count = self.items.len() as isize;
        let current = self.hover.map_or(if delta > 0 { -1 } else { count }, |h| h as isize);
        self.hover = Some((current + delta).rem_euclid(count) as usize);
    }
}

pub struct BlockMenuLayout {
    pub panel: Rect,
    pub rows: Vec<Rect>,
}

pub fn menu_layout(menu: &BlockMenu, m: CellMetrics, scale: f32, window: Rect) -> BlockMenuLayout {
    let px = |v: f32| (v * scale).round();
    let row = (m.height * 1.6).round();
    let pad = px(6.0);
    let longest = menu.items.iter().map(|a| text_columns(a.label(menu.folded))).max().unwrap_or(10);
    let width = ((longest + 4) as f32 * m.width).max(px(200.0));
    let height = menu.items.len() as f32 * row + 2.0 * pad;
    let x = menu.x.min(window.x + window.width - width - pad).max(window.x + pad);
    let y = menu.y.min(window.y + window.height - height - pad).max(window.y + pad);
    let rows = (0..menu.items.len())
        .map(|i| Rect::new(x + pad, y + pad + i as f32 * row, width - 2.0 * pad, row))
        .collect();
    BlockMenuLayout {
        panel: Rect::new(x, y, width, height),
        rows,
    }
}

/// The item under a point. `Some(None)` = inside the menu but on nothing.
pub fn menu_hit(layout: &BlockMenuLayout, x: f32, y: f32) -> Option<Option<usize>> {
    if !layout.panel.contains(x, y) {
        return None;
    }
    Some(layout.rows.iter().position(|r| r.contains(x, y)))
}

pub fn draw_menu(menu: &BlockMenu, layout: &BlockMenuLayout, theme: &Theme, m: CellMetrics, scale: f32, out: &mut Vec<ChromeItem>) {
    let bg = theme.background.0;
    let fg = theme.foreground.0;
    let panel_bg = mix(bg, fg, 0.09);
    let p = layout.panel;
    let radius = (8.0 * scale).round();
    let edge = scale.round().max(1.0);
    out.push(ChromeItem::RoundedRect {
        rect: [p.x - edge, p.y - edge, p.width + 2.0 * edge, p.height + 2.0 * edge],
        radius: radius + edge,
        top_only: false,
        color: mix(bg, fg, 0.22),
        alpha: 1.0,
    });
    out.push(ChromeItem::RoundedRect {
        rect: [p.x, p.y, p.width, p.height],
        radius,
        top_only: false,
        color: panel_bg,
        alpha: 1.0,
    });
    for (i, (action, r)) in menu.items.iter().zip(&layout.rows).enumerate() {
        if menu.hover == Some(i) {
            out.push(ChromeItem::RoundedRect {
                rect: [r.x, r.y, r.width, r.height],
                radius: (5.0 * scale).round(),
                top_only: false,
                color: mix(panel_bg, fg, 0.12),
                alpha: 1.0,
            });
        }
        out.push(ChromeItem::Text {
            text: action.label(menu.folded).into(),
            x: r.x + (8.0 * scale).round(),
            y: r.y + ((r.height - m.height) / 2.0).round(),
            color: fg,
            bold: false,
        });
    }
}

/// The block handle in a pane's left padding: three dots beside the
/// block's first visible row, and a faint line along the rest of it.
#[allow(clippy::too_many_arguments)]
pub fn draw_handle(area: Rect, origin: [f32; 2], rows: (isize, isize), hovered: bool, color: Rgb, m: CellMetrics, scale: f32, out: &mut Vec<ChromeItem>) {
    let padding = origin[0] - area.x;
    if padding < 3.0 * scale {
        return;
    }
    let (first, last) = rows;
    let line_x = area.x + (padding / 2.0).round() - scale.round().max(1.0) / 2.0;
    let top = origin[1] + first as f32 * m.height;
    let bottom = origin[1] + (last + 1) as f32 * m.height;
    out.push(ChromeItem::Rect {
        rect: [line_x, top, scale.round().max(1.0), bottom - top],
        color,
        alpha: if hovered { 0.5 } else { 0.25 },
    });
    // The dots, on the first row.
    let dot = (2.0 * scale).round().max(2.0);
    let gap = (2.0 * scale).round();
    let dots_height = 3.0 * dot + 2.0 * gap;
    let x = area.x + ((padding - dot) / 2.0).round();
    let y = top + ((m.height - dots_height) / 2.0).round();
    for i in 0..3 {
        out.push(ChromeItem::RoundedRect {
            rect: [x, y + i as f32 * (dot + gap), dot, dot],
            radius: dot / 2.0,
            top_only: false,
            color,
            alpha: if hovered { 1.0 } else { 0.7 },
        });
    }
}

//#endregion

#[cfg(test)]
mod tests {
    use super::*;
    use ionterm_vt::{Palette, Processor};

    fn session() -> (Term, Processor) {
        let mut t = Term::new(30, 6, 100, Palette::default());
        let mut p = Processor::new();
        p.advance(&mut t, b"\x1b]133;A\x07$ \x1b]133;B\x07seq 12\r\n\x1b]133;C\x07");
        for n in 1..=12 {
            p.advance(&mut t, format!("{n}\r\n").as_bytes());
        }
        p.advance(&mut t, b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        (t, p)
    }

    #[test]
    fn finds_blocks() {
        let (t, _) = session();
        let block = block_at(&t, 5).unwrap();
        assert_eq!(block.prompt, 0);
        assert_eq!(block.command.as_deref(), Some("seq 12"));
        assert_eq!(block.output, Some((1, 12)));
        assert_eq!(block.end, 12);
        assert!(block.finished);
        // At the prompt, actions go to the last command that ran.
        assert_eq!(current_block(&t, false).map(|b| b.prompt), Some(0));
        assert!(!block_at(&t, 13).unwrap().finished);
    }

    #[test]
    fn sticky_header_only_when_scrolled_into_output() {
        let (mut t, _) = session();
        // At the bottom and idle: no header.
        assert_eq!(sticky(&t, None), None);
        t.scroll_display(3);
        let header = sticky(&t, None).unwrap();
        assert_eq!(header.text, "seq 12");
        assert_eq!(header.status, Some(0));
        // Scrolled all the way up the prompt is on screen itself.
        t.scroll_display(100);
        assert_eq!(sticky(&t, None), None);
    }

    #[test]
    fn running_commands_get_a_header_at_the_bottom() {
        let mut t = Term::new(30, 4, 100, Palette::default());
        let mut p = Processor::new();
        p.advance(&mut t, b"\x1b]133;A\x07$ \x1b]133;B\x07make\r\n\x1b]133;C\x07");
        for n in 1..=10 {
            p.advance(&mut t, format!("step {n}\r\n").as_bytes());
        }
        let header = sticky(&t, Some(Duration::from_secs(63))).unwrap();
        assert_eq!(header.text, "make");
        assert_eq!(header.detail, "running 1m 03s");
        assert_eq!(header.status, None);
    }

    #[test]
    fn formats_markdown_and_durations() {
        assert_eq!(markdown(Some("ls"), "a\nb"), "```console\n$ ls\na\nb\n```\n");
        assert!(markdown(None, "```x```").starts_with("````\n"));
        assert_eq!(duration(Duration::from_millis(850)), "850ms");
        assert_eq!(duration(Duration::from_secs(7_500)), "2h 05m");
    }

    #[test]
    fn menus_offer_what_applies() {
        let (t, _) = session();
        let menu = BlockMenu::new(1, block_at(&t, 3).unwrap(), false, 0.0, 0.0).unwrap();
        assert!(menu.items.contains(&BlockAction::Rerun));
        assert!(menu.items.contains(&BlockAction::Fold));
        // The command line being typed has nothing yet.
        assert!(BlockMenu::new(1, block_at(&t, 13).unwrap(), false, 0.0, 0.0).is_none());
    }

    #[test]
    fn scrollbar_geometry() {
        let track = Rect::new(0.0, 0.0, 6.0, 100.0);
        assert_eq!(scrollbar_thumb(track, 100, 0, 25, 1.0), (0.0, 25.0));
        assert_eq!(scrollbar_thumb(track, 100, 75, 25, 1.0), (75.0, 25.0));
        assert_eq!(scrollbar_line(track, 50.0, 100, 20), 40);
    }
}
