//! Re-wrapping text when the terminal width changes.
//!
//! Soft-wrapped rows are joined back into logical lines and split again at the
//! new width, so a long command or log line keeps reading as one line after a
//! resize instead of being cut off.

use crate::cell::{Cell, CommandRun, Flags, Row};
use crate::color::Color;
use crate::graphics::Anchor;

/// Shell integration marks, which belong to a line's first row.
#[derive(Clone, Copy, Default)]
struct Marks {
    prompt: bool,
    output: bool,
    status: Option<i32>,
    ran: Option<CommandRun>,
}

impl Marks {
    fn of(row: &Row) -> Self {
        Self {
            prompt: row.prompt,
            output: row.output,
            status: row.status,
            ran: row.ran,
        }
    }
}

/// A line as it was before wrapping: its cells and what was attached to them.
struct LogicalLine {
    cells: Vec<Cell>,
    /// The cursor's offset into `cells`, if it's on this line.
    cursor: Option<usize>,
    marks: Marks,
    /// Image anchors by their offset into `cells`.
    anchors: Vec<(usize, Anchor)>,
}

pub struct Reflowed {
    pub rows: Vec<Row>,
    /// The cursor's new (row, col) in `rows`.
    pub cursor: (usize, usize),
}

/// Re-wraps `rows` (oldest first) to `cols` columns. `cursor` is a (row, col)
/// index into `rows`, and comes back pointing at the same character.
pub fn reflow(rows: Vec<Row>, cursor: (usize, usize), cols: usize) -> Reflowed {
    let cursor_row = cursor.0.min(rows.len().saturating_sub(1));

    // Join soft-wrapped rows into logical lines, remembering where the cursor
    // is and which lines start a prompt.
    let mut lines: Vec<LogicalLine> = Vec::new();
    let mut current: Vec<Cell> = Vec::new();
    let mut marks = Marks::default();
    // Image anchors by their offset into the logical line.
    let mut anchors: Vec<(usize, Anchor)> = Vec::new();
    let mut cursor_offset = None;
    let mut cursor_line = 0;
    for (index, row) in rows.into_iter().enumerate() {
        let row_marks = Marks::of(&row);
        let mut cells = row.cells;
        if row.wrapped && cells.last().is_some_and(is_wide_padding) {
            cells.pop();
        }
        if index == cursor_row {
            cursor_offset = Some(current.len() + cursor.1);
            cursor_line = lines.len();
        }
        if current.is_empty() {
            marks = row_marks;
        }
        anchors.extend(row.anchors.iter().map(|a| (current.len() + a.col, *a)));
        current.extend(cells);
        if !row.wrapped {
            let keep = cursor_offset.map_or(0, |offset| offset + 1);
            trim_trailing_blanks(&mut current, keep);
            lines.push(LogicalLine {
                cells: std::mem::take(&mut current),
                cursor: cursor_offset.take(),
                marks,
                anchors: std::mem::take(&mut anchors),
            });
        }
    }
    if !current.is_empty() || cursor_offset.is_some() || !anchors.is_empty() {
        let keep = cursor_offset.map_or(0, |offset| offset + 1);
        trim_trailing_blanks(&mut current, keep);
        lines.push(LogicalLine {
            cells: current,
            cursor: cursor_offset,
            marks,
            anchors,
        });
    }
    // Empty lines below the cursor are just unused screen, not content.
    while lines.len() > cursor_line + 1
        && lines.last().is_some_and(|l| l.cells.is_empty() && l.anchors.is_empty())
    {
        lines.pop();
    }

    // Split every logical line at the new width.
    let mut out = Vec::new();
    let mut new_cursor = (0, 0);
    for line in lines {
        let LogicalLine {
            cells,
            cursor: cursor_offset,
            marks,
            anchors: mut line_anchors,
        } = line;
        let len = cells.len();
        let first_row = out.len();
        let mut row: Vec<Cell> = Vec::with_capacity(cols);
        let mut row_anchors: Vec<Anchor> = Vec::new();
        line_anchors.sort_by_key(|(offset, _)| *offset);
        let mut pending = line_anchors.into_iter().peekable();
        for (offset, cell) in cells.into_iter().enumerate() {
            let wide = cell.flags.contains(Flags::WIDE);
            let full = row.len() == cols || (wide && row.len() + 1 == cols && cols > 1);
            if full {
                if row.len() < cols {
                    // The wide character goes to the next row as a whole.
                    row.push(wide_padding());
                }
                let first = out.len() == first_row;
                out.push(Row {
                    cells: std::mem::take(&mut row),
                    wrapped: true,
                    prompt: marks.prompt && first,
                    output: marks.output && first,
                    status: marks.status.filter(|_| first),
                    ran: marks.ran.filter(|_| first),
                    anchors: std::mem::take(&mut row_anchors),
                });
            }
            if cursor_offset == Some(offset) {
                new_cursor = (out.len(), row.len());
            }
            while let Some((_, anchor)) = pending.next_if(|(at, _)| *at <= offset) {
                row_anchors.push(Anchor { col: row.len(), ..anchor });
            }
            row.push(cell);
        }
        // Anchors past the line's (trimmed) end stay on its last row.
        for (at, anchor) in pending {
            let col = (row.len() + at.saturating_sub(len)).min(cols - 1);
            row_anchors.push(Anchor { col, ..anchor });
        }
        if let Some(offset) = cursor_offset
            && offset >= len
        {
            new_cursor = (out.len(), (row.len() + offset - len).min(cols - 1));
        }
        row.resize(cols, Cell::blank(Color::Default));
        let first = out.len() == first_row;
        out.push(Row {
            cells: row,
            wrapped: false,
            prompt: marks.prompt && first,
            output: marks.output && first,
            status: marks.status.filter(|_| first),
                    ran: marks.ran.filter(|_| first),
            anchors: row_anchors,
        });
    }
    if out.is_empty() {
        out.push(Row::new(cols, Color::Default));
    }
    Reflowed {
        rows: out,
        cursor: new_cursor,
    }
}

/// The blank left at the end of a row when a wide character didn't fit.
pub fn wide_padding() -> Cell {
    Cell {
        flags: Flags::WIDE_SPACER,
        ..Cell::default()
    }
}

/// A wide-character spacer with no wide character before it: padding, not text.
fn is_wide_padding(cell: &Cell) -> bool {
    cell.flags == Flags::WIDE_SPACER && cell.c == ' '
}

/// Drops default-coloured blanks from the end of a line, keeping at least
/// `keep` cells so the cursor's position survives.
fn trim_trailing_blanks(cells: &mut Vec<Cell>, keep: usize) {
    while cells.len() > keep
        && cells.last().is_some_and(|c| {
            c.c == ' ' && c.extra.is_none() && c.bg == Color::Default && (c.flags.is_empty() || is_wide_padding(c))
        })
    {
        cells.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str, cols: usize, wrapped: bool) -> Row {
        let mut cells: Vec<Cell> = text
            .chars()
            .map(|c| Cell {
                c,
                ..Cell::default()
            })
            .collect();
        cells.resize(cols, Cell::default());
        Row {
            cells,
            wrapped,
            prompt: false,
            output: false,
            status: None,
            ran: None,
            anchors: Vec::new(),
        }
    }

    fn text(row: &Row) -> String {
        row.cells.iter().map(|c| c.c).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn rewraps_wider_and_narrower() {
        let rows = vec![row("hello", 5, true), row("world", 5, false), row("$", 5, false)];
        let wide = reflow(rows, (2, 2), 10);
        assert_eq!(wide.rows.iter().map(text).collect::<Vec<_>>(), ["helloworld", "$"]);
        assert_eq!(wide.cursor, (1, 2));

        let narrow = reflow(wide.rows, wide.cursor, 4);
        assert_eq!(narrow.rows.iter().map(text).collect::<Vec<_>>(), ["hell", "owor", "ld", "$"]);
        assert!(narrow.rows[0].wrapped && narrow.rows[1].wrapped && !narrow.rows[2].wrapped);
        assert_eq!(narrow.cursor, (3, 2));
    }

    #[test]
    fn keeps_wide_characters_whole() {
        let mut cells = vec![
            Cell { c: 'a', ..Cell::default() },
            Cell { c: '字', flags: Flags::WIDE, ..Cell::default() },
            Cell { c: ' ', flags: Flags::WIDE_SPACER, ..Cell::default() },
        ];
        cells.resize(4, Cell::default());
        let out = reflow(
            vec![Row {
                cells,
                wrapped: false,
                prompt: false,
                output: false,
                status: None,
                ran: None,
                anchors: Vec::new(),
            }],
            (0, 3),
            2,
        );
        assert_eq!(out.rows[0].cells[0].c, 'a');
        assert!(is_wide_padding(&out.rows[0].cells[1]));
        assert!(out.rows[1].cells[0].flags.contains(Flags::WIDE));
        // And joining back drops the padding again.
        let back = reflow(out.rows, out.cursor, 4);
        assert_eq!(back.rows[0].cells[1].c, '字');
    }

    #[test]
    fn anchors_follow_their_cell() {
        let mut rows = vec![row("abcdef", 6, true), row("gh", 6, false)];
        rows[1].anchors.push(Anchor { placement: 9, col: 1 });
        // Offset 7 ("h") lands on row 1, column 3 at width 4.
        let out = reflow(rows, (1, 2), 4);
        assert_eq!(out.rows[1].anchors, vec![Anchor { placement: 9, col: 3 }]);
    }

    #[test]
    fn drops_empty_screen_below_the_cursor() {
        let rows = vec![row("$ ls", 6, false), row("", 6, false), row("", 6, false)];
        let out = reflow(rows, (0, 4), 8);
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.cursor, (0, 4));
    }
}
