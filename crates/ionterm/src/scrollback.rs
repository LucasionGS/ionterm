//! The scrollback in a pager (ctrl+shift+h, like kitty's `show_scrollback`):
//! the pane's whole history written out with its colours, for `less` or
//! whatever `[terminal] scrollback_pager` names.

use std::fmt::Write;
use std::path::{Path, PathBuf};

use ionterm_vt::{Color, Flags, Term};

/// The pager's placeholder for the line to open at.
const LINE_PLACEHOLDER: &str = "INPUT_LINE_NUMBER";

/// The scrollback as text with SGR colours, and the line (from 1) that a
/// pager `pager_rows` tall should open at to show what's on screen.
pub fn export(term: &Term, pager_rows: usize) -> (String, usize) {
    let total = term.history_len() + term.rows();
    // Blank lines below the last output would only push it up in the pager.
    let end = (0..total)
        .rev()
        .find(|&i| term.line(i).is_some_and(|row| row.cells.iter().any(|c| !is_blank(c))))
        .map_or(0, |i| i + 1);
    let top = term.viewport_to_absolute(0);

    let mut out = String::new();
    let mut line: usize = 1;
    let mut top_line = 1;
    let mut style = Style::default();
    for absolute in 0..end {
        let Some(row) = term.line(absolute) else { break };
        if absolute == top {
            top_line = line;
        }
        let used = if row.wrapped {
            row.cells.len()
        } else {
            row.cells.iter().rposition(|c| !is_blank(c)).map_or(0, |i| i + 1)
        };
        for cell in &row.cells[..used] {
            if cell.flags.contains(Flags::WIDE_SPACER) {
                continue;
            }
            let next = Style::of(cell);
            if next != style {
                next.write_sgr(&mut out);
                style = next;
            }
            out.push_str(&cell.text());
        }
        if !row.wrapped {
            // Reset at each line end so `less` doesn't carry colours over.
            if style != Style::default() {
                out.push_str("\x1b[0m");
                style = Style::default();
            }
            out.push('\n');
            line += 1;
        }
    }
    // At the bottom the pager shows the last screenful (less keeps a row
    // for its prompt), as the terminal does.
    if term.display_offset() == 0 {
        top_line = line.saturating_sub(pager_rows.saturating_sub(1)).max(1);
    }
    (out, top_line)
}

fn file_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

fn file_prefix() -> String {
    format!("ionterm-scrollback-{}-", std::process::id())
}

/// Writes the export to a file only this user can read.
pub fn write_file(text: &str, pane: u64) -> std::io::Result<PathBuf> {
    let path = file_dir().join(format!("{}{pane}.txt", file_prefix()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(&path)?, text.as_bytes())?;
    Ok(path)
}

/// Removes this process's exports. The pager's shell removes its file when
/// it quits, but not when ionterm closes first and takes it down.
pub fn remove_files() {
    let prefix = file_prefix();
    let Ok(entries) = std::fs::read_dir(file_dir()) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The command showing `path` in `pager` at `line`. On Unix it runs through
/// `sh`, which removes the file once the pager quits.
pub fn pager_command(pager: &str, path: &Path, line: usize) -> Vec<String> {
    let pager = pager.replace(LINE_PLACEHOLDER, &line.to_string());
    let path = path.to_string_lossy().into_owned();
    if cfg!(unix) {
        vec!["sh".into(), "-c".into(), format!("{pager} \"$1\"; rm -f -- \"$1\""), "sh".into(), path]
    } else {
        pager.split_whitespace().map(str::to_string).chain([path]).collect()
    }
}

fn is_blank(cell: &ionterm_vt::Cell) -> bool {
    let text = cell.text();
    (text.is_empty() || text == " ") && cell.bg == Color::Default && !cell.flags.contains(Flags::INVERSE)
}

/// The attributes that change how text looks in a pager.
#[derive(Clone, Copy, Default, PartialEq)]
struct Style {
    fg: Color,
    bg: Color,
    flags: Flags,
}

impl Style {
    const SHOWN: Flags = Flags::BOLD
        .union(Flags::DIM)
        .union(Flags::ITALIC)
        .union(Flags::ALL_UNDERLINES)
        .union(Flags::INVERSE)
        .union(Flags::STRIKE);

    fn of(cell: &ionterm_vt::Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags & Self::SHOWN,
        }
    }

    fn write_sgr(&self, out: &mut String) {
        out.push_str("\x1b[0");
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::ALL_UNDERLINES, "4"),
            (Flags::INVERSE, "7"),
            (Flags::STRIKE, "9"),
        ] {
            if self.flags.intersects(flag) {
                let _ = write!(out, ";{code}");
            }
        }
        write_color(out, self.fg, 30, 90, 38);
        write_color(out, self.bg, 40, 100, 48);
        out.push('m');
    }
}

fn write_color(out: &mut String, color: Color, base: u8, bright: u8, extended: u8) {
    let _ = match color {
        Color::Default => Ok(()),
        Color::Indexed(n @ 0..8) => write!(out, ";{}", base + n),
        Color::Indexed(n @ 8..16) => write!(out, ";{}", bright + n - 8),
        Color::Indexed(n) => write!(out, ";{extended};5;{n}"),
        Color::Rgb(c) => write!(out, ";{extended};2;{};{};{}", c.r, c.g, c.b),
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use ionterm_vt::{Palette, Processor};

    #[test]
    fn exports_colours_and_joins_wrapped_lines() {
        let mut term = Term::new(10, 4, 100, Palette::default());
        let mut vt = Processor::new();
        vt.advance(&mut term, b"plain\r\n\x1b[1;31mred\x1b[0m done\r\n0123456789abc\r\n");
        let (text, line) = export(&term, 4);
        assert_eq!(text, "plain\n\x1b[0;1;31mred\x1b[0m done\n0123456789abc\n");
        assert_eq!(line, 1);
    }

    #[test]
    fn opens_at_the_screen() {
        let mut term = Term::new(10, 3, 100, Palette::default());
        let mut vt = Processor::new();
        for n in 0..20 {
            vt.advance(&mut term, format!("line {n}\r\n").as_bytes());
        }
        // 20 lines (the empty one below is dropped); less shows rows - 1.
        assert_eq!(export(&term, 3).1, 19);
        assert_eq!(export(&term, 2).1, 20);
        term.scroll_display(5);
        let top = term.viewport_to_absolute(0);
        assert_eq!(export(&term, 3).1, top + 1);
    }

    #[test]
    fn pager_command_fills_the_line() {
        let argv = pager_command("less -R +INPUT_LINE_NUMBER", Path::new("/tmp/x"), 42);
        if cfg!(unix) {
            assert_eq!(argv[2], "less -R +42 \"$1\"; rm -f -- \"$1\"");
            assert_eq!(argv[4], "/tmp/x");
        }
    }
}
