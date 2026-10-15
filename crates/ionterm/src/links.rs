//! Finding the link under the mouse: an OSC 8 hyperlink, or a plain URL in
//! the text, and opening it.

use ionterm_vt::{Flags, Term};

/// Schemes ionterm opens. Anything else (a `javascript:` or custom handler
/// URI printed by a program) is ignored rather than handed to the desktop.
const OPENABLE_SCHEMES: &[&str] = &["http://", "https://", "file://", "ftp://", "mailto:"];
/// Schemes recognised in plain text.
const TEXT_SCHEMES: &[&str] = &["https://", "http://", "file://", "ftp://", "mailto:"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// Every cell carrying this OSC 8 id.
    Hyperlink(u32),
    /// A URL found in the text, between two absolute positions (inclusive).
    Text((usize, usize), (usize, usize)),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub uri: String,
    pub kind: LinkKind,
}

/// The link at an absolute position, if any.
pub fn link_at(term: &Term, line: usize, col: usize) -> Option<Link> {
    let cell = term.line(line)?.cells.get(col)?;
    if cell.hyperlink != 0 {
        let uri = term.hyperlink(cell.hyperlink)?.to_string();
        return Some(Link {
            uri,
            kind: LinkKind::Hyperlink(cell.hyperlink),
        });
    }
    url_at(term, line, col)
}

/// Finds a plain-text URL covering the position, following soft wraps so a
/// URL broken across rows is still found whole.
fn url_at(term: &Term, line: usize, col: usize) -> Option<Link> {
    let mut first = line;
    while first > 0 && term.line(first - 1).is_some_and(|row| row.wrapped) {
        first -= 1;
    }
    let mut last = line;
    while term.line(last).is_some_and(|row| row.wrapped) && term.line(last + 1).is_some() {
        last += 1;
    }

    // The logical line's characters with their positions.
    let mut chars = Vec::new();
    for index in first..=last {
        let Some(row) = term.line(index) else { break };
        for (c, cell) in row.cells.iter().enumerate() {
            if !cell.flags.contains(Flags::WIDE_SPACER) {
                chars.push((cell.c, (index, c)));
            }
        }
    }
    let hovered = chars.iter().position(|&(_, pos)| pos == (line, col))?;
    let text: Vec<char> = chars.iter().map(|&(c, _)| c).collect();

    let mut start = 0;
    while start < text.len() {
        let Some(scheme_len) = TEXT_SCHEMES.iter().find_map(|scheme| {
            let s: Vec<char> = scheme.chars().collect();
            text[start..].starts_with(&s).then_some(s.len())
        }) else {
            start += 1;
            continue;
        };
        let mut end = start + scheme_len;
        while end < text.len() && is_url_char(text[end]) {
            end += 1;
        }
        end = trim_url_end(&text[start..end]) + start;
        if end > start + scheme_len && (start..end).contains(&hovered) {
            return Some(Link {
                uri: text[start..end].iter().collect(),
                kind: LinkKind::Text(chars[start].1, chars[end - 1].1),
            });
        }
        start = end.max(start + 1);
    }
    None
}

fn is_url_char(c: char) -> bool {
    !c.is_whitespace() && !c.is_control() && !matches!(c, '<' | '>' | '"' | '`' | '{' | '}' | '|' | '\\' | '^')
}

/// Drops trailing punctuation that belongs to the sentence, not the URL. A
/// closing bracket stays when the URL opened one (`wiki/Foo_(bar)`).
fn trim_url_end(url: &[char]) -> usize {
    let mut end = url.len();
    while end > 0 {
        let c = url[end - 1];
        let unbalanced = |open: char, close: char| {
            let opens = url[..end].iter().filter(|&&x| x == open).count();
            let closes = url[..end].iter().filter(|&&x| x == close).count();
            c == close && closes > opens
        };
        if matches!(c, '.' | ',' | ':' | ';' | '!' | '?' | '\'') || unbalanced('(', ')') || unbalanced('[', ']') {
            end -= 1;
        } else {
            break;
        }
    }
    end
}

/// Opens a link with the desktop's handler, without waiting for it.
pub fn open(uri: &str) {
    if !OPENABLE_SCHEMES.iter().any(|scheme| uri.starts_with(scheme)) {
        tracing::warn!("not opening link with an unsupported scheme: {uri}");
        return;
    }
    #[cfg(windows)]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", uri]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(uri);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(uri);
        c
    };
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match command.spawn() {
        // Reap it in the background so it doesn't linger as a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(error) => tracing::warn!("could not open {uri}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ionterm_vt::{Palette, Processor};

    fn term_with(text: &str, cols: usize) -> Term {
        let mut term = Term::new(cols, 4, 100, Palette::default());
        Processor::new().advance(&mut term, text.as_bytes());
        term
    }

    #[test]
    fn finds_urls_and_trims_punctuation() {
        let term = term_with("see https://ionnet.io/a_(b). ok", 40);
        let link = link_at(&term, 0, 10).unwrap();
        assert_eq!(link.uri, "https://ionnet.io/a_(b)");
        assert!(link_at(&term, 0, 1).is_none());
    }

    #[test]
    fn follows_soft_wraps() {
        let term = term_with("https://example.com/long", 10);
        let link = link_at(&term, 2, 1).unwrap();
        assert_eq!(link.uri, "https://example.com/long");
        assert_eq!(link.kind, LinkKind::Text((0, 0), (2, 3)));
    }
}
