//! The tab bar: Windows Terminal-style tabs or kitty's Powerline strip,
//! laid out here and drawn with the renderer's chrome primitives.

use ionterm_render::{CellMetrics, ChromeItem, text_columns};
use ionterm_vt::Rgb;

use crate::config::TabBarStyleConfig;
use crate::layout::Rect;
use crate::theme::Theme;

/// What the tab bar shows for one tab.
pub struct TabInfo {
    pub title: String,
    pub icon: char,
    pub color: Option<Rgb>,
    pub active: bool,
    pub progress: Option<TabProgress>,
}

/// A progress bar along the bottom of a tab.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TabProgress {
    /// 0 to 1; `None` when the program can't tell (a moving stripe).
    pub fraction: Option<f32>,
    pub state: ionterm_vt::ProgressState,
}

/// Draws a tab's progress bar inside `rect`, along its bottom edge.
/// `phase` (0 to 1) moves the stripe of an indeterminate one.
pub fn draw_progress(progress: TabProgress, rect: Rect, theme: &Theme, scale: f32, phase: f32, out: &mut Vec<ChromeItem>) {
    use ionterm_vt::ProgressState;
    let height = (2.5 * scale).round().max(2.0);
    let inset = (8.0 * scale).round();
    let (x, width) = (rect.x + inset, (rect.width - 2.0 * inset).max(0.0));
    let y = rect.y + rect.height - height - (2.0 * scale).round();
    let palette = theme.palette();
    let color = match progress.state {
        ProgressState::Error => palette.colors[1],
        ProgressState::Paused => palette.colors[3],
        _ => theme.cursor.0,
    };
    out.push(ChromeItem::Rect {
        rect: [x, y, width, height],
        color: mix(theme.background.0, theme.foreground.0, 0.2),
        alpha: 1.0,
    });
    let (start, length) = match progress.fraction {
        Some(fraction) => (0.0, width * fraction.clamp(0.0, 1.0)),
        None => {
            // A third of the track, sliding through and off the end.
            let stripe = width / 3.0;
            let start = (width + stripe) * phase - stripe;
            (start.max(0.0), (start + stripe).min(width) - start.max(0.0))
        }
    };
    if length > 0.0 {
        out.push(ChromeItem::Rect {
            rect: [x + start, y, length, height],
            color,
            alpha: 1.0,
        });
    }
}

/// What's under the mouse in the tab bar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Hit {
    #[default]
    None,
    /// The bar's empty space.
    Bar,
    Tab(usize),
    Close(usize),
    NewTab,
    /// The `⌄` beside `+`: the connection picker.
    Dropdown,
}

#[derive(Clone, Copy)]
pub struct TabSlot {
    pub rect: Rect,
    pub close: Option<Rect>,
    pub icon_x: f32,
    pub text_x: f32,
    pub text_y: f32,
    /// Columns available for the title.
    pub text_cols: usize,
}

pub struct BarLayout {
    pub bar: Rect,
    pub tabs: Vec<TabSlot>,
    pub new_tab: Option<Rect>,
    pub dropdown: Option<Rect>,
}

/// The rename in progress, drawn in place of a tab's title.
pub struct RenameView<'a> {
    pub tab: usize,
    pub text: &'a str,
}

pub fn bar_height(style: TabBarStyleConfig, m: CellMetrics) -> f32 {
    match style {
        TabBarStyleConfig::Tabs => (m.height * 1.75).round(),
        _ => m.height,
    }
}

pub fn layout(style: TabBarStyleConfig, bar: Rect, tabs: &[TabInfo], m: CellMetrics, scale: f32, max_title: usize) -> BarLayout {
    match style {
        TabBarStyleConfig::Tabs => layout_tabs(bar, tabs.len(), m, scale),
        _ => layout_powerline(bar, tabs, m, max_title),
    }
}

fn layout_tabs(bar: Rect, count: usize, m: CellMetrics, scale: f32) -> BarLayout {
    let px = |v: f32| (v * scale).round();
    let top = px(6.0);
    let side = px(6.0);
    let gap = px(2.0);
    let height = bar.height - top;
    let y = bar.y + top;
    let button = height;
    let dropdown_width = (button * 0.7).round();
    let available = bar.width - 2.0 * side - button - dropdown_width - gap;
    let max_width = m.width * 30.0;
    let min_width = m.width * 8.0;
    let width = (available / count.max(1) as f32 - gap).clamp(min_width, max_width).floor();

    let pad = px(10.0);
    let close_size = (m.height * 1.15).round();
    let tabs = (0..count)
        .map(|i| {
            let x = bar.x + side + i as f32 * (width + gap);
            let rect = Rect::new(x, y, width, height);
            let close = Rect::new(
                x + width - pad * 0.6 - close_size,
                y + ((height - close_size) / 2.0).round(),
                close_size,
                close_size,
            );
            let icon_x = x + pad;
            let text_x = icon_x + 2.0 * m.width;
            let text_cols = ((close.x - px(4.0) - text_x) / m.width).max(0.0) as usize;
            TabSlot {
                rect,
                close: Some(close),
                icon_x,
                text_x,
                text_y: y + ((height - m.height) / 2.0).round(),
                text_cols,
            }
        })
        .collect::<Vec<_>>();
    let after = tabs.last().map_or(bar.x + side, |t| t.rect.x + t.rect.width + gap);
    let new_tab = Rect::new(after, y + px(2.0), button - px(4.0), button - px(4.0));
    BarLayout {
        bar,
        tabs,
        new_tab: Some(new_tab),
        dropdown: Some(Rect::new(new_tab.x + new_tab.width, new_tab.y, dropdown_width, new_tab.height)),
    }
}

fn layout_powerline(bar: Rect, tabs: &[TabInfo], m: CellMetrics, max_title: usize) -> BarLayout {
    let mut x = bar.x;
    let slots = tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let label = powerline_label(i, tab, max_title);
            let width = (text_columns(&label) + 1) as f32 * m.width;
            let slot = TabSlot {
                rect: Rect::new(x, bar.y, width, bar.height),
                close: None,
                icon_x: x,
                text_x: x,
                text_y: bar.y,
                text_cols: text_columns(&label),
            };
            x += width;
            slot
        })
        .collect();
    BarLayout {
        bar,
        tabs: slots,
        new_tab: None,
        dropdown: None,
    }
}

fn powerline_label(index: usize, tab: &TabInfo, max_title: usize) -> String {
    format!(" {} {} {} ", index + 1, tab.icon, truncate(&tab.title, max_title))
}

impl TabSlot {
    /// The same slot moved sideways, for animation.
    fn shifted(&self, dx: f32) -> TabSlot {
        let mut slot = *self;
        slot.rect.x += dx;
        slot.icon_x += dx;
        slot.text_x += dx;
        if let Some(close) = &mut slot.close {
            close.x += dx;
        }
        slot
    }
}

/// How tabs are displaced from their slots while animating: an x offset per
/// tab, and the tab being dragged, which is drawn above the others.
pub struct Motion<'a> {
    pub offsets: &'a [f32],
    pub raised: Option<usize>,
}

impl Motion<'_> {
    fn offset(&self, i: usize) -> f32 {
        self.offsets.get(i).copied().unwrap_or(0.0)
    }

    /// Drawing order: everything else first, the raised tab last.
    fn order(&self, count: usize) -> Vec<usize> {
        let mut order: Vec<usize> = (0..count).filter(|&i| Some(i) != self.raised).collect();
        order.extend(self.raised.filter(|&i| i < count));
        order
    }
}

pub fn hit(layout: &BarLayout, x: f32, y: f32) -> Hit {
    if !layout.bar.contains(x, y) {
        return Hit::None;
    }
    if layout.new_tab.is_some_and(|r| r.contains(x, y)) {
        return Hit::NewTab;
    }
    if layout.dropdown.is_some_and(|r| r.contains(x, y)) {
        return Hit::Dropdown;
    }
    for (i, slot) in layout.tabs.iter().enumerate() {
        if slot.close.is_some_and(|r| r.contains(x, y)) {
            return Hit::Close(i);
        }
        if slot.rect.contains(x, y) {
            return Hit::Tab(i);
        }
    }
    Hit::Bar
}

/// The tab slot under an x position, for dragging: clamps to the ends.
pub fn slot_at_x(layout: &BarLayout, x: f32) -> usize {
    let last = layout.tabs.len().saturating_sub(1);
    layout
        .tabs
        .iter()
        .position(|slot| x < slot.rect.x + slot.rect.width)
        .unwrap_or(last)
}

#[allow(clippy::too_many_arguments)]
pub fn draw(
    style: TabBarStyleConfig,
    layout: &BarLayout,
    tabs: &[TabInfo],
    hover: Hit,
    rename: Option<RenameView>,
    motion: &Motion,
    theme: &Theme,
    opacity: f32,
    m: CellMetrics,
    scale: f32,
    max_title: usize,
    phase: f32,
    out: &mut Vec<ChromeItem>,
) {
    match style {
        TabBarStyleConfig::Tabs => draw_tabs(layout, tabs, hover, rename, motion, theme, opacity, m, scale, phase, out),
        _ => draw_powerline(style, layout, tabs, rename, motion, theme, m, scale, max_title, phase, out),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_tabs(
    layout: &BarLayout,
    tabs: &[TabInfo],
    hover: Hit,
    rename: Option<RenameView>,
    motion: &Motion,
    theme: &Theme,
    opacity: f32,
    m: CellMetrics,
    scale: f32,
    phase: f32,
    out: &mut Vec<ChromeItem>,
) {
    let background = theme.background.0;
    let foreground = theme.foreground.0;
    // The strip sits a shade lighter than the terminal; the active tab
    // takes the terminal's own colour so it reads as one surface.
    let strip = mix(background, foreground, 0.07);
    let hover_bg = mix(background, foreground, 0.13);
    let muted = mix(foreground, strip, 0.35);
    let radius = (7.0 * scale).round();
    let bar = layout.bar;
    out.push(ChromeItem::Rect {
        rect: [bar.x, bar.y, bar.width, bar.height],
        color: strip,
        alpha: opacity,
    });

    let still = motion.offsets.iter().all(|dx| *dx == 0.0);
    for i in motion.order(tabs.len().min(layout.tabs.len())) {
        let tab = &tabs[i];
        let slot = &layout.tabs[i].shifted(motion.offset(i));
        let hovered = matches!(hover, Hit::Tab(h) | Hit::Close(h) if h == i);
        let r = slot.rect;
        let (bg, alpha) = match (tab.color, tab.active) {
            (Some(color), true) => (color, 1.0),
            (Some(color), false) => (mix(strip, color, if hovered { 0.6 } else { 0.4 }), 1.0),
            (None, true) => (background, opacity),
            (None, false) if hovered => (hover_bg, opacity),
            (None, false) => (strip, 0.0),
        };
        if alpha > 0.0 {
            out.push(ChromeItem::RoundedRect {
                rect: [r.x, r.y, r.width, r.height],
                radius,
                top_only: true,
                color: bg,
                alpha,
            });
        }
        // A thin divider between neighbouring plain inactive tabs.
        let next_plain = tabs
            .get(i + 1)
            .is_some_and(|n| !n.active && n.color.is_none() && !matches!(hover, Hit::Tab(h) | Hit::Close(h) if h == i + 1));
        if still && !tab.active && tab.color.is_none() && !hovered && next_plain {
            let line_h = (r.height * 0.45).round();
            out.push(ChromeItem::Rect {
                rect: [r.x + r.width, r.y + ((r.height - line_h) / 2.0).round(), scale.round().max(1.0), line_h],
                color: mix(strip, foreground, 0.2),
                alpha: 1.0,
            });
        }

        let fg = match tab.color {
            Some(color) if tab.active || hovered => readable_on(color),
            _ if tab.active || hovered => foreground,
            _ => muted,
        };
        out.push(ChromeItem::Text {
            text: tab.icon.to_string(),
            x: slot.icon_x,
            y: slot.text_y,
            color: fg,
            bold: false,
        });
        match &rename {
            Some(edit) if edit.tab == i => {
                let shown = tail(edit.text, slot.text_cols.saturating_sub(1));
                out.push(ChromeItem::Text {
                    text: shown.clone(),
                    x: slot.text_x,
                    y: slot.text_y,
                    color: fg,
                    bold: false,
                });
                let caret_x = slot.text_x + text_columns(&shown) as f32 * m.width;
                out.push(ChromeItem::Rect {
                    rect: [caret_x, slot.text_y, (2.0 * scale).round(), m.height],
                    color: theme.cursor.0,
                    alpha: 1.0,
                });
            }
            _ => out.push(ChromeItem::Text {
                text: truncate(&tab.title, slot.text_cols),
                x: slot.text_x,
                y: slot.text_y,
                color: fg,
                bold: false,
            }),
        }

        if let Some(close) = slot.close
            && (tab.active || hovered)
        {
            if hover == Hit::Close(i) {
                out.push(ChromeItem::RoundedRect {
                    rect: [close.x, close.y, close.width, close.height],
                    radius: (4.0 * scale).round(),
                    top_only: false,
                    color: mix(bg, foreground, 0.18),
                    alpha: 1.0,
                });
            }
            out.push(ChromeItem::Text {
                text: "×".into(),
                x: close.x + ((close.width - m.width) / 2.0).round(),
                y: close.y + ((close.height - m.height) / 2.0).round(),
                color: fg,
                bold: false,
            });
        }
        if let Some(progress) = tab.progress {
            draw_progress(progress, r, theme, scale, phase, out);
        }
    }

    if let Some(button) = layout.new_tab {
        if hover == Hit::NewTab {
            out.push(ChromeItem::RoundedRect {
                rect: [button.x, button.y, button.width, button.height],
                radius: (5.0 * scale).round(),
                top_only: false,
                color: hover_bg,
                alpha: 1.0,
            });
        }
        out.push(ChromeItem::Text {
            text: "+".into(),
            x: button.x + ((button.width - m.width) / 2.0).round(),
            y: button.y + ((button.height - m.height) / 2.0).round(),
            color: if hover == Hit::NewTab { foreground } else { muted },
            bold: false,
        });
    }
    if let Some(button) = layout.dropdown {
        if hover == Hit::Dropdown {
            out.push(ChromeItem::RoundedRect {
                rect: [button.x, button.y, button.width, button.height],
                radius: (5.0 * scale).round(),
                top_only: false,
                color: hover_bg,
                alpha: 1.0,
            });
        }
        out.push(ChromeItem::Text {
            text: "\u{F078}".into(),
            x: button.x + ((button.width - m.width) / 2.0).round(),
            y: button.y + ((button.height - m.height) / 2.0).round(),
            color: if hover == Hit::Dropdown { foreground } else { muted },
            bold: false,
        });
    }
}

/// kitty-style Powerline tabs: the separator glyph in the tab's colour over
/// the next tab's.
#[allow(clippy::too_many_arguments)]
fn draw_powerline(
    style: TabBarStyleConfig,
    layout: &BarLayout,
    tabs: &[TabInfo],
    rename: Option<RenameView>,
    motion: &Motion,
    theme: &Theme,
    m: CellMetrics,
    scale: f32,
    max_title: usize,
    phase: f32,
    out: &mut Vec<ChromeItem>,
) {
    let colors = &theme.tab_bar;
    let bar = layout.bar;
    out.push(ChromeItem::Rect {
        rect: [bar.x, bar.y, bar.width, bar.height],
        color: colors.background.0,
        alpha: 1.0,
    });
    let separator = match style {
        TabBarStyleConfig::Angled => '\u{E0B0}',
        TabBarStyleConfig::Round => '\u{E0B4}',
        _ => '\u{E0BC}',
    };
    let tab_bg = |tab: &TabInfo| match (tab.color, tab.active) {
        (Some(color), _) => color,
        (None, true) => colors.active_background.0,
        (None, false) => colors.inactive_background.0,
    };
    for i in motion.order(tabs.len().min(layout.tabs.len())) {
        let tab = &tabs[i];
        let slot = &layout.tabs[i].shifted(motion.offset(i));
        let bg = tab_bg(tab);
        let fg = match tab.color {
            Some(color) => readable_on(color),
            None if tab.active => colors.active_foreground.0,
            None => colors.inactive_foreground.0,
        };
        let r = slot.rect;
        out.push(ChromeItem::Rect {
            rect: [r.x, r.y, r.width - m.width, r.height],
            color: bg,
            alpha: 1.0,
        });
        let label = match &rename {
            Some(edit) if edit.tab == i => format!(" {} {} {}_ ", i + 1, tab.icon, edit.text),
            _ => powerline_label(i, tab, max_title),
        };
        out.push(ChromeItem::Text {
            text: label,
            x: r.x,
            y: r.y,
            color: fg,
            bold: tab.active,
        });
        if let Some(progress) = tab.progress {
            draw_progress(progress, Rect::new(r.x, r.y, r.width - m.width, r.height + (2.0 * scale).round()), theme, scale, phase, out);
        }
        let next_bg = tabs.get(i + 1).map_or(colors.background.0, tab_bg);
        let sep_x = r.x + r.width - m.width;
        out.push(ChromeItem::Rect {
            rect: [sep_x, r.y, m.width, r.height],
            color: next_bg,
            alpha: 1.0,
        });
        out.push(ChromeItem::Text {
            text: separator.to_string(),
            x: sep_x,
            y: r.y,
            color: bg,
            bold: false,
        });
    }
}

/// A Nerd Font icon for the program running in a tab.
pub fn process_icon(process: Option<&str>) -> char {
    match process.unwrap_or_default() {
        "fish" | "bash" | "zsh" | "sh" | "dash" | "nu" | "pwsh" | "powershell" | "cmd" | "elvish" | "xonsh" => '\u{F489}',
        "nvim" | "vim" | "vi" | "hx" | "helix" | "nano" | "micro" | "kak" => '\u{E62B}',
        "ssh" | "mosh" | "mosh-client" | "sshpass" => '\u{F233}',
        "docker" | "podman" | "lazydocker" => '\u{F308}',
        "python" | "python3" | "ipython" => '\u{E73C}',
        "node" | "npm" | "npx" | "bun" | "deno" => '\u{E718}',
        "git" | "lazygit" | "tig" | "gitui" => '\u{E702}',
        "htop" | "btop" | "top" | "btm" | "nvtop" => '\u{F080}',
        "cargo" | "rustc" | "rust-analyzer" => '\u{E7A8}',
        "go" => '\u{E627}',
        "man" | "less" | "more" | "bat" => '\u{F02D}',
        "yazi" | "ranger" | "lf" | "nnn" | "ifm" | "mc" => '\u{F07B}',
        "mariadb" | "mysql" | "psql" | "sqlite3" => '\u{F1C0}',
        _ => '\u{F120}',
    }
}

pub fn truncate(text: &str, cols: usize) -> String {
    if text_columns(text) <= cols {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = text_columns(&c.to_string());
        if used + w + 1 > cols {
            break;
        }
        out.push(c);
        used += w;
    }
    if cols > 0 {
        out.push('…');
    }
    out
}

/// The end of a string that fits in `cols` columns (for text being typed).
fn tail(text: &str, cols: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut start = chars.len();
    let mut used = 0;
    while start > 0 {
        let w = text_columns(&chars[start - 1].to_string());
        if used + w > cols {
            break;
        }
        used += w;
        start -= 1;
    }
    chars[start..].iter().collect()
}

pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let lerp = |x: u8, y: u8| (f32::from(x) * (1.0 - t) + f32::from(y) * t).round() as u8;
    Rgb::new(lerp(a.r, b.r), lerp(a.g, b.g), lerp(a.b, b.b))
}

/// Black or white, whichever reads better on `bg`.
pub fn readable_on(bg: Rgb) -> Rgb {
    let luminance = 0.2126 * f32::from(bg.r) + 0.7152 * f32::from(bg.g) + 0.0722 * f32::from(bg.b);
    if luminance > 140.0 { Rgb::new(0x10, 0x10, 0x14) } else { Rgb::new(0xf5, 0xf5, 0xf7) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> CellMetrics {
        CellMetrics {
            width: 9.0,
            height: 20.0,
            baseline: 15.0,
            underline_offset: 2.0,
            underline_thickness: 1.0,
            strikeout_offset: 5.0,
        }
    }

    #[test]
    fn tabs_share_space_and_hit_test() {
        let bar = Rect::new(0.0, 0.0, 800.0, 35.0);
        let layout = layout_tabs(bar, 3, metrics(), 1.0);
        assert_eq!(layout.tabs.len(), 3);
        let first = &layout.tabs[0];
        assert!(first.rect.width <= 9.0 * 30.0);
        let close = first.close.unwrap();
        assert_eq!(hit(&layout, close.x + 2.0, close.y + 2.0), Hit::Close(0));
        assert_eq!(hit(&layout, first.rect.x + 3.0, first.rect.y + 3.0), Hit::Tab(0));
        let plus = layout.new_tab.unwrap();
        assert_eq!(hit(&layout, plus.x + 3.0, plus.y + 3.0), Hit::NewTab);
        assert_eq!(slot_at_x(&layout, 10_000.0), 2);
    }

    #[test]
    fn truncates_titles() {
        assert_eq!(truncate("fish ~/development", 8), "fish ~/…");
        assert_eq!(truncate("fish", 8), "fish");
        assert_eq!(tail("abcdef", 3), "def");
    }
}
