//! Hints mode: finding what's worth picking on screen (URLs, file paths,
//! `file:line`, git hashes, IP addresses), labelling it, and working out
//! how to open a file. The window's side is `app/hints.rs`.

use std::path::{Path, PathBuf};

use crate::links::{TEXT_SCHEMES, is_url_char, trim_url_end};

/// Label letters, easiest to reach first: the home row, then the rows
/// above and below.
const ALPHABET: &str = "asdfjklghqweruiopzxcvbnmty";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HintKind {
    Url,
    /// A file, with the line (and column) a tool pointed at.
    FileLine { path: String, line: u32, col: Option<u32> },
    Path,
    Hash,
    Ip,
}

/// Something found in a line of text: `start..end` are indices into it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub start: usize,
    pub end: usize,
    pub kind: HintKind,
}

/// Finds everything hintable in one (logical) line. `exists` says whether
/// a path is a real file or folder; `None` when that can't be checked (a
/// remote shell), so only paths that look like paths count.
pub fn scan_line(text: &[char], exists: Option<&dyn Fn(&str) -> bool>) -> Vec<Found> {
    let mut found = urls(text);
    for (start, end) in tokens(text) {
        if found.iter().any(|f| start < f.end && f.start < end) {
            continue;
        }
        let token: String = text[start..end].iter().collect();
        if let Some(kind) = classify(&token, exists) {
            found.push(Found { start, end, kind });
        }
    }
    found.sort_by_key(|f| f.start);
    found
}

/// URLs with a scheme, as the hover detection finds them.
fn urls(text: &[char]) -> Vec<Found> {
    let mut found = Vec::new();
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
        if end > start + scheme_len {
            found.push(Found {
                start,
                end,
                kind: HintKind::Url,
            });
        }
        start = end.max(start + 1);
    }
    found
}

/// Words, split at spaces and the punctuation paths don't contain, with
/// sentence punctuation trimmed off the end.
fn tokens(text: &[char]) -> Vec<(usize, usize)> {
    let breaks = |c: char| c.is_whitespace() || "\"'`()[]{}<>,;|=".contains(c);
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if breaks(text[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < text.len() && !breaks(text[i]) {
            i += 1;
        }
        let mut end = i;
        while end > start && matches!(text[end - 1], '.' | ':' | '!' | '?') {
            end -= 1;
        }
        if end > start {
            out.push((start, end));
        }
    }
    out
}

fn classify(token: &str, exists: Option<&dyn Fn(&str) -> bool>) -> Option<HintKind> {
    if is_ip(token) {
        return Some(HintKind::Ip);
    }
    if let Some((path, line, col)) = file_line(token)
        && looks_like_file(path)
        && exists.is_none_or(|exists| exists(path))
    {
        return Some(HintKind::FileLine {
            path: path.to_string(),
            line,
            col,
        });
    }
    if is_hash(token) {
        return Some(HintKind::Hash);
    }
    let path_chars = token.chars().all(|c| c.is_alphanumeric() || "/._-~+@%#".contains(c));
    if path_chars && token.chars().any(char::is_alphanumeric) {
        let real = match exists {
            // Checked: any word that names a file or folder here counts.
            Some(exists) => exists(token),
            // Unchecked: only what's clearly a path.
            None => token.contains('/') && (token.starts_with(['/', '~', '.']) || has_extension(token)),
        };
        if real {
            return Some(HintKind::Path);
        }
    }
    None
}

/// `path:line` or `path:line:col`.
fn file_line(token: &str) -> Option<(&str, u32, Option<u32>)> {
    let mut parts = token.rsplitn(3, ':');
    let last = parts.next()?;
    let middle = parts.next()?;
    match parts.next() {
        Some(path) => Some((path, middle.parse().ok()?, Some(last.parse().ok()?))),
        None => Some((middle, last.parse().ok()?, None)),
    }
}

/// A file name with an extension, or something with a slash in it.
fn looks_like_file(path: &str) -> bool {
    !path.is_empty() && !path.contains(':') && (path.contains('/') || has_extension(path))
}

/// Whether the last part of a path ends in a file extension (`.rs`, `.json`).
fn has_extension(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && (1..=10).contains(&ext.len()) && ext.chars().all(char::is_alphanumeric))
}

/// A git hash: 7 to 40 lowercase hex digits, with both digits and letters
/// (so plain numbers and hex-looking words don't count).
fn is_hash(token: &str) -> bool {
    (7..=40).contains(&token.len())
        && token.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        && token.chars().any(|c| c.is_ascii_digit())
        && token.chars().any(|c| c.is_ascii_alphabetic())
}

/// An IPv4 address, maybe with a port.
fn is_ip(token: &str) -> bool {
    let (address, port) = token.split_once(':').unwrap_or((token, ""));
    let parts: Vec<&str> = address.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| !p.is_empty() && p.len() <= 3 && p.parse::<u8>().is_ok())
        && (port.is_empty() || port.parse::<u16>().is_ok())
}

/// Labels for `count` hints, all the same length so none is the start of
/// another. The first goes to the most recent output.
pub fn labels(count: usize) -> Vec<String> {
    let alphabet: Vec<char> = ALPHABET.chars().collect();
    let mut length = 1;
    while alphabet.len().pow(length) < count {
        length += 1;
    }
    (0..count)
        .map(|mut n| {
            let mut label = String::new();
            for _ in 0..length {
                label.insert(0, alphabet[n % alphabet.len()]);
                n /= alphabet.len();
            }
            label
        })
        .collect()
}

//#region Opening files

/// How to open a file someone picked.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenFile {
    /// A terminal editor: run it in a new tab.
    Tab(Vec<String>),
    /// A program with its own window: start it and leave it be.
    Detached(Vec<String>),
}

/// The command for opening `path` (at `line`, `col`): the configured
/// template (`code -g {path}:{line}`), or else `$VISUAL`/`$EDITOR`.
pub fn open_file_command(template: &str, path: &Path, line: Option<u32>, col: Option<u32>) -> Option<OpenFile> {
    let path_text = path.to_string_lossy();
    if !template.trim().is_empty() {
        let fill = |word: &str| {
            word.replace("{path}", &path_text)
                .replace("{line}", &line.unwrap_or(1).to_string())
                .replace("{col}", &col.unwrap_or(1).to_string())
        };
        let mut argv: Vec<String> = template.split_whitespace().map(fill).collect();
        if !template.contains("{path}") {
            argv.push(path_text.into_owned());
        }
        return Some(OpenFile::Detached(argv));
    }
    editor_command(editor()?, path, line, col)
}

/// How a given editor opens `path` at `line` and `col`.
fn editor_command(editor: Vec<String>, path: &Path, line: Option<u32>, col: Option<u32>) -> Option<OpenFile> {
    let path_text = path.to_string_lossy();
    let name = Path::new(&editor[0]).file_name()?.to_string_lossy().into_owned();
    let mut argv = editor;
    let at = |separator: &str| match (line, col) {
        (Some(line), Some(col)) => format!("{path_text}{separator}{line}{separator}{col}"),
        (Some(line), None) => format!("{path_text}{separator}{line}"),
        _ => path_text.to_string(),
    };
    Some(match name.as_str() {
        "code" | "codium" | "code-oss" | "vscodium" => {
            argv.extend(["-g".into(), at(":")]);
            OpenFile::Detached(argv)
        }
        "subl" | "zed" | "kate" => {
            argv.push(at(":"));
            OpenFile::Detached(argv)
        }
        "hx" | "helix" => {
            argv.push(at(":"));
            OpenFile::Tab(argv)
        }
        // vim and nvim can put the cursor on the column too.
        "vim" | "nvim" if line.is_some() && col.is_some() => {
            argv.push(format!("+call cursor({}, {})", line.unwrap_or(1), col.unwrap_or(1)));
            argv.push(path_text.into_owned());
            OpenFile::Tab(argv)
        }
        _ => {
            if let Some(line) = line {
                argv.push(format!("+{line}"));
            }
            argv.push(path_text.into_owned());
            OpenFile::Tab(argv)
        }
    })
}

/// The user's editor: `$VISUAL`, `$EDITOR`, or the first common one found.
fn editor() -> Option<Vec<String>> {
    let from_env = ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|value| !value.trim().is_empty());
    if let Some(value) = from_env {
        return Some(value.split_whitespace().map(String::from).collect());
    }
    ["nvim", "vim", "nano", "vi"]
        .iter()
        .find(|name| in_path(name))
        .map(|name| vec![name.to_string()])
}

fn in_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
}

/// A picked path made absolute: `~` is home, relative paths are from the
/// shell's directory.
pub fn resolve(path: &str, cwd: Option<&Path>) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    let path = Path::new(path);
    match cwd {
        Some(cwd) if path.is_relative() => cwd.join(path),
        _ => path.to_path_buf(),
    }
}

//#endregion

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str, exists: Option<&dyn Fn(&str) -> bool>) -> Vec<(String, HintKind)> {
        let text: Vec<char> = line.chars().collect();
        scan_line(&text, exists)
            .into_iter()
            .map(|f| (text[f.start..f.end].iter().collect(), f.kind))
            .collect()
    }

    #[test]
    fn finds_what_matters_without_a_filesystem() {
        let found = kinds("error at src/main.rs:12:5: see https://x.io/a_(b). commit 3f2a9c1d on 10.0.0.1:22.", None);
        assert_eq!(
            found,
            vec![
                (
                    "src/main.rs:12:5".into(),
                    HintKind::FileLine {
                        path: "src/main.rs".into(),
                        line: 12,
                        col: Some(5)
                    }
                ),
                ("https://x.io/a_(b)".into(), HintKind::Url),
                ("3f2a9c1d".into(), HintKind::Hash),
                ("10.0.0.1:22".into(), HintKind::Ip),
            ]
        );
        // Words, numbers and slashes in prose aren't paths without a check.
        assert!(kinds("and/or 1234567 deadbeef v1.2", None).is_empty());
        assert_eq!(kinds("cat ./notes /etc/hosts", None).len(), 2);
    }

    #[test]
    fn checks_paths_where_it_can() {
        let exists = |p: &str| matches!(p, "Cargo.toml" | "src" | "README.md");
        let found = kinds("Cargo.toml  src  target  README.md:3", Some(&exists));
        let texts: Vec<_> = found.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(texts, ["Cargo.toml", "src", "README.md:3"]);
    }

    #[test]
    fn labels_never_prefix_each_other() {
        assert_eq!(labels(3), ["a", "s", "d"]);
        let many = labels(30);
        assert!(many.iter().all(|l| l.len() == 2));
        assert_eq!(&many[..2], ["aa", "as"]);
        let unique: std::collections::HashSet<_> = many.iter().collect();
        assert_eq!(unique.len(), 30);
    }

    #[test]
    fn builds_editor_commands() {
        let path = Path::new("/src/main.rs");
        assert_eq!(
            open_file_command("code -g {path}:{line}", path, Some(4), None),
            Some(OpenFile::Detached(vec!["code".into(), "-g".into(), "/src/main.rs:4".into()]))
        );
        // The editor's own way of going to a line (and column).
        assert_eq!(
            editor_command(vec!["nvim".into()], path, Some(4), Some(2)),
            Some(OpenFile::Tab(vec!["nvim".into(), "+call cursor(4, 2)".into(), "/src/main.rs".into()]))
        );
        assert_eq!(
            editor_command(vec!["hx".into()], path, Some(4), None),
            Some(OpenFile::Tab(vec!["hx".into(), "/src/main.rs:4".into()]))
        );
        assert_eq!(
            editor_command(vec!["nano".into()], path, Some(4), Some(2)),
            Some(OpenFile::Tab(vec!["nano".into(), "+4".into(), "/src/main.rs".into()]))
        );
        assert_eq!(
            open_file_command("xdg-open", path, None, None),
            Some(OpenFile::Detached(vec!["xdg-open".into(), "/src/main.rs".into()]))
        );
    }
}
