//! The tab context menu: rename, duplicate, colour and close.

use ionterm_render::{CellMetrics, ChromeItem};
use ionterm_vt::Rgb;

use crate::layout::Rect;
use crate::tabbar::mix;
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    Rename,
    Duplicate,
    /// `None` removes the colour.
    Color(Option<Rgb>),
    Close,
    CloseOthers,
}

/// An open menu for one tab.
pub struct Menu {
    pub tab: usize,
    /// Top-left corner, where it was opened.
    pub x: f32,
    pub y: f32,
    pub hover: Option<MenuCommand>,
}

pub struct MenuLayout {
    pub panel: Rect,
    pub entries: Vec<(MenuCommand, Rect, &'static str)>,
    pub swatches: Vec<(MenuCommand, Rect)>,
    /// The "Color" label row.
    pub color_label: Rect,
}

/// The colours offered: the theme's ANSI hues and its accent.
fn swatch_colors(theme: &Theme) -> Vec<Rgb> {
    let palette = theme.palette();
    [1, 3, 2, 6, 4, 5]
        .iter()
        .map(|&i| palette.colors[i])
        .chain([theme.cursor.0])
        .collect()
}

pub fn layout(menu: &Menu, theme: &Theme, m: CellMetrics, scale: f32, window: Rect) -> MenuLayout {
    let px = |v: f32| (v * scale).round();
    let row = (m.height * 1.6).round();
    let pad = px(6.0);
    let width = (m.width * 24.0).max(px(220.0));
    let rows = 6.0; // rename, duplicate, colour label, swatches, close, close others
    let height = rows * row + 2.0 * pad + px(8.0);
    // Keep the menu inside the window.
    let x = menu.x.min(window.x + window.width - width - pad).max(window.x + pad);
    let y = menu.y.min(window.y + window.height - height - pad).max(window.y);
    let panel = Rect::new(x, y, width, height);

    let item = |i: f32, extra: f32| Rect::new(x + pad, y + pad + i * row + extra, width - 2.0 * pad, row);
    let separator = px(8.0);
    let entries = vec![
        (MenuCommand::Rename, item(0.0, 0.0), "Rename tab"),
        (MenuCommand::Duplicate, item(1.0, 0.0), "Duplicate tab"),
        (MenuCommand::Close, item(4.0, separator), "Close tab"),
        (MenuCommand::CloseOthers, item(5.0, separator), "Close other tabs"),
    ];
    // Row 2 is the "Colour" label, row 3 the swatches.
    let color_label = item(2.0, 0.0);

    let colors = swatch_colors(theme);
    let size = (row * 0.75).round();
    let swatch_row = item(3.0, 0.0);
    let step = size + px(6.0);
    let mut swatches: Vec<(MenuCommand, Rect)> = colors
        .into_iter()
        .enumerate()
        .map(|(i, color)| {
            let r = Rect::new(
                swatch_row.x + px(8.0) + i as f32 * step,
                swatch_row.y + ((row - size) / 2.0).round(),
                size,
                size,
            );
            (MenuCommand::Color(Some(color)), r)
        })
        .collect();
    let none_x = swatch_row.x + px(8.0) + swatches.len() as f32 * step;
    swatches.push((
        MenuCommand::Color(None),
        Rect::new(none_x, swatch_row.y + ((row - size) / 2.0).round(), size, size),
    ));

    MenuLayout {
        panel,
        entries,
        swatches,
        color_label,
    }
}

/// The command under a point. `Some(None)` = inside the menu but on nothing.
pub fn hit(layout: &MenuLayout, x: f32, y: f32) -> Option<Option<MenuCommand>> {
    if !layout.panel.contains(x, y) {
        return None;
    }
    let found = layout
        .entries
        .iter()
        .find(|(_, r, _)| r.contains(x, y))
        .map(|(c, _, _)| *c)
        .or_else(|| layout.swatches.iter().find(|(_, r)| r.contains(x, y)).map(|(c, _)| *c));
    Some(found)
}

pub fn draw(menu: &Menu, layout: &MenuLayout, theme: &Theme, m: CellMetrics, scale: f32, out: &mut Vec<ChromeItem>) {
    let bg = theme.background.0;
    let fg = theme.foreground.0;
    let panel_bg = mix(bg, fg, 0.09);
    let p = layout.panel;
    let radius = (8.0 * scale).round();
    // A one-pixel outline: a slightly larger rounded rect underneath.
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

    let text_y = |r: &Rect| r.y + ((r.height - m.height) / 2.0).round();
    for (command, r, label) in &layout.entries {
        if menu.hover == Some(*command) {
            out.push(ChromeItem::RoundedRect {
                rect: [r.x, r.y, r.width, r.height],
                radius: (5.0 * scale).round(),
                top_only: false,
                color: mix(panel_bg, fg, 0.12),
                alpha: 1.0,
            });
        }
        out.push(ChromeItem::Text {
            text: (*label).to_string(),
            x: r.x + (8.0 * scale).round(),
            y: text_y(r),
            color: fg,
            bold: false,
        });
    }
    // Divider above the close entries.
    if let Some((_, close, _)) = layout.entries.iter().find(|(c, _, _)| *c == MenuCommand::Close) {
        out.push(ChromeItem::Rect {
            rect: [p.x + (6.0 * scale).round(), close.y - (4.0 * scale).round(), p.width - (12.0 * scale).round(), edge],
            color: mix(panel_bg, fg, 0.15),
            alpha: 1.0,
        });
    }
    let label = layout.color_label;
    out.push(ChromeItem::Text {
        text: "Colour".into(),
        x: label.x + (8.0 * scale).round(),
        y: text_y(&label),
        color: mix(fg, panel_bg, 0.35),
        bold: false,
    });

    for (command, r) in &layout.swatches {
        let hovered = menu.hover == Some(*command);
        if hovered {
            let ring = (2.0 * scale).round();
            out.push(ChromeItem::RoundedRect {
                rect: [r.x - ring, r.y - ring, r.width + 2.0 * ring, r.height + 2.0 * ring],
                radius: r.width / 2.0 + ring,
                top_only: false,
                color: fg,
                alpha: 1.0,
            });
        }
        match command {
            MenuCommand::Color(Some(color)) => out.push(ChromeItem::RoundedRect {
                rect: [r.x, r.y, r.width, r.height],
                radius: r.width / 2.0,
                top_only: false,
                color: *color,
                alpha: 1.0,
            }),
            _ => {
                // "No colour": a hollow circle with a slash.
                out.push(ChromeItem::RoundedRect {
                    rect: [r.x, r.y, r.width, r.height],
                    radius: r.width / 2.0,
                    top_only: false,
                    color: mix(panel_bg, fg, 0.45),
                    alpha: 1.0,
                });
                let inset = edge * 1.5;
                out.push(ChromeItem::RoundedRect {
                    rect: [r.x + inset, r.y + inset, r.width - 2.0 * inset, r.height - 2.0 * inset],
                    radius: r.width / 2.0 - inset,
                    top_only: false,
                    color: panel_bg,
                    alpha: 1.0,
                });
                out.push(ChromeItem::Text {
                    text: "╱".into(),
                    x: r.x + ((r.width - m.width) / 2.0).round(),
                    y: r.y + ((r.height - m.height) / 2.0).round(),
                    color: mix(panel_bg, fg, 0.6),
                    bold: false,
                });
            }
        }
    }
}
