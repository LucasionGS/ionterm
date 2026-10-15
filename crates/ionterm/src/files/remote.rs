//! Commands run on the remote host for the file panel: listing, renaming,
//! deleting, and the streams behind uploads and downloads.
//!
//! Each operation is one `ssh <host> <command>` over the tab's shared
//! connection, so it starts instantly and never asks for a password. The
//! command is a POSIX `sh` script sent base64-encoded (like the bootstrap),
//! so paths with spaces or quotes and login shells like fish don't matter.

use std::io::Read;
use std::process::{Child, Command, Stdio};

use crate::ssh::Connection;
use crate::ssh::bootstrap::base64;

/// A host the file panel talks to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub alias: String,
    /// `ssh … <alias>`, to which the remote command is appended.
    argv: Vec<String>,
}

impl Remote {
    pub fn new(connection: &Connection) -> Self {
        Self {
            alias: connection.alias.clone(),
            argv: connection.exec_argv(),
        }
    }

    /// For tests and local experiments: a "remote" that is any command
    /// taking a shell command as its last argument (like `sh -c`).
    #[cfg(test)]
    pub fn with_argv(argv: Vec<String>) -> Self {
        Self {
            alias: "test".into(),
            argv,
        }
    }

    /// For tests against a real host: `argv` ends in `-- <alias>`.
    #[cfg(test)]
    pub fn with_alias(alias: &str, argv: Vec<String>) -> Self {
        Self {
            alias: alias.into(),
            argv,
        }
    }

    /// The `ssh …` options without the host, for tools that run ssh
    /// themselves (rsync's `-e`). `None` for test "remotes" that aren't ssh.
    pub fn ssh_options(&self) -> Option<&[String]> {
        match self.argv.as_slice() {
            [options @ .., separator, alias] if separator == "--" && *alias == self.alias => Some(options),
            _ => None,
        }
    }

    /// A command that runs `script` with `sh` on the host.
    pub fn command(&self, script: &str) -> Command {
        let mut command = Command::new(&self.argv[0]);
        command.args(&self.argv[1..]).arg(wrap_script(script));
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        hide_console(&mut command);
        command
    }

    /// Runs a script to completion and returns its output, or a readable
    /// error: the script's own message, or ssh's.
    pub fn run(&self, script: &str) -> Result<Vec<u8>, String> {
        let output = self
            .command(script)
            .output()
            .map_err(|error| format!("Could not run ssh: {error}"))?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(error_message(&output.stderr, output.status.code()))
        }
    }

    /// Lists a directory (`None` = the home directory).
    pub fn list(&self, dir: Option<&str>) -> Result<Listing, String> {
        let output = self.run(&list_script(dir))?;
        parse_listing(&output).ok_or_else(|| "The server's directory listing could not be read.".to_string())
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        let (from, to) = (quote(from), quote(to));
        self.run(&format!(
            "if [ -e {to} ] || [ -L {to} ]; then echo 'Something with that name already exists.' >&2; exit 1; fi\nmv -- {from} {to}"
        ))
        .map(drop)
    }

    /// Deletes files and folders, folders with everything in them.
    pub fn delete(&self, paths: &[String]) -> Result<(), String> {
        let quoted: Vec<String> = paths.iter().map(|p| quote(p)).collect();
        self.run(&format!("rm -rf -- {}", quoted.join(" "))).map(drop)
    }

    pub fn mkdir(&self, path: &str) -> Result<(), String> {
        let path = quote(path);
        self.run(&format!(
            "if [ -e {path} ]; then echo 'Something with that name already exists.' >&2; exit 1; fi\nmkdir -p -- {path}"
        ))
        .map(drop)
    }

    /// Which of `names` already exist in `dir`.
    pub fn existing(&self, dir: &str, names: &[String]) -> Result<Vec<String>, String> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let mut script = format!("cd -- {} || exit 1\n", quote(dir));
        for name in names {
            let q = quote(name);
            script.push_str(&format!("if [ -e {q} ] || [ -L {q} ]; then printf '%s\\0' {q}; fi\n"));
        }
        let output = self.run(&script)?;
        Ok(split_nul(&output).map(|s| String::from_utf8_lossy(s).into_owned()).collect())
    }

    /// Total size in bytes of a remote folder's files, when `du` can tell.
    pub fn folder_size(&self, path: &str) -> Option<u64> {
        let path = quote(path);
        // GNU du counts bytes; BusyBox and BSD only kilobytes.
        let script = format!("du -sb -- {path} 2>/dev/null || {{ du -sk -- {path} | sed 's/^\\([0-9]*\\).*/\\1K/'; }}");
        let output = self.run(&script).ok()?;
        let text = String::from_utf8_lossy(&output);
        let field = text.split_whitespace().next()?;
        match field.strip_suffix('K') {
            Some(kb) => kb.parse::<u64>().ok().map(|k| k * 1024),
            None => field.parse().ok(),
        }
    }

    /// Starts streaming a remote file to stdout.
    pub fn spawn_cat(&self, path: &str) -> std::io::Result<Child> {
        self.command(&format!("exec cat -- {}", quote(path))).spawn()
    }

    /// Starts streaming a remote folder as a tar archive to stdout.
    pub fn spawn_tar_out(&self, path: &str) -> std::io::Result<Child> {
        let (parent, name) = split_path(path);
        self.command(&format!("cd -- {} && exec tar -cf - -- {}", quote(parent), quote(name))).spawn()
    }

    /// Starts writing stdin to a remote file: into `<path>.part` first, then
    /// renamed over `path`, so an interrupted upload never leaves half a file
    /// under the real name.
    pub fn spawn_upload(&self, path: &str) -> std::io::Result<Child> {
        let (path, part) = (quote(path), quote(&format!("{path}.part")));
        let mut command = self.command(&format!("cat > {part} && mv -f -- {part} {path}"));
        command.stdin(Stdio::piped());
        command.spawn()
    }

    /// Starts extracting a tar archive from stdin inside `dir`.
    pub fn spawn_tar_in(&self, dir: &str) -> std::io::Result<Child> {
        let dir = quote(dir);
        let mut command = self.command(&format!("mkdir -p -- {dir} && cd -- {dir} && exec tar -xf -"));
        command.stdin(Stdio::piped());
        command.spawn()
    }

    /// Removes what an interrupted upload left behind.
    pub fn remove_part(&self, path: &str) {
        let _ = self.run(&format!("rm -f -- {}", quote(&format!("{path}.part"))));
    }
}

/// Reads a finished child's stderr into a readable error.
pub fn child_error(child: &mut Child, code: Option<i32>) -> String {
    let mut stderr = Vec::new();
    if let Some(pipe) = child.stderr.as_mut() {
        let _ = pipe.read_to_end(&mut stderr);
    }
    error_message(&stderr, code)
}

/// The last meaningful line of stderr, or a generic message.
fn error_message(stderr: &[u8], code: Option<i32>) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().map(str::trim).rfind(|l| !l.is_empty());
    match (line, code) {
        (Some(line), _) => line.trim_start_matches("sh: ").to_string(),
        // ssh itself exits with 255 when it can't connect.
        (None, Some(255)) => "Could not reach the server.".into(),
        (None, Some(code)) => format!("The command failed (exit {code})."),
        (None, None) => "The command was interrupted.".into(),
    }
}

/// The remote command that runs `script` with `sh`, whatever the login
/// shell is: only letters, digits and `+/=` go inside the quotes.
fn wrap_script(script: &str) -> String {
    format!("exec sh -c 'eval \"$(printf %s {} | base64 -d)\"'", base64(script.as_bytes()))
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW: no console flashing up for every listing.
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

/// Quotes a string for `sh`: single quotes, with `'` written as `'\''`.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Joins a remote directory and a name.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// The parent directory of a remote path (`/` stays `/`).
pub fn parent(path: &str) -> String {
    split_path(path).0.to_string()
}

/// Splits a remote path into its directory and its last component.
fn split_path(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => ("/", &trimmed[1..]),
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None if trimmed.is_empty() => ("/", ""),
        None => (".", trimmed),
    }
}

fn split_nul(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|&b| b == 0).filter(|s| !s.is_empty())
}

//#region Listing

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    /// A symlink, and whether it points at a folder.
    Link { to_dir: bool },
    /// Sockets, pipes, devices.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    /// In bytes.
    pub size: u64,
    /// Seconds since the Unix epoch, when the listing had it.
    pub mtime: Option<i64>,
    /// Like `rwxr-xr-x`.
    pub mode: String,
    pub owner: String,
    /// Where a symlink points.
    pub target: Option<String>,
}

impl Entry {
    /// Whether opening it navigates into it.
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::Link { to_dir: true })
    }

    pub fn is_hidden(&self) -> bool {
        self.name.starts_with('.')
    }

    /// Whether the owner may run it (`-rwx…`).
    pub fn is_executable(&self) -> bool {
        self.mode.as_bytes().get(3) == Some(&b'x')
    }
}

/// A directory's contents, folders first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    /// The directory's absolute path, as the server resolved it.
    pub path: String,
    pub entries: Vec<Entry>,
}

/// The listing script. It prints the resolved directory on the first line,
/// then `F` and NUL-separated `find -printf` fields where GNU find exists,
/// or `L` and `ls -la` output elsewhere (BusyBox, BSD).
fn list_script(dir: Option<&str>) -> String {
    let cd = match dir {
        Some(dir) => format!("cd -- {} || exit 1", quote(dir)),
        None => "cd || exit 1".into(),
    };
    format!(
        r#"{cd}
pwd
if find . -maxdepth 0 -printf '' >/dev/null 2>&1; then
  echo F
  exec find . -mindepth 1 -maxdepth 1 -printf '%y\0%Y\0%s\0%T@\0%m\0%u\0%f\0%l\0'
fi
echo L
export LC_ALL=C
if ls -la --time-style=+%s . >/dev/null 2>&1; then exec ls -la --time-style=+%s
elif ls -laD %s . >/dev/null 2>&1; then exec ls -laD %s
else exec ls -la
fi
"#
    )
}

fn parse_listing(output: &[u8]) -> Option<Listing> {
    let first = output.iter().position(|&b| b == b'\n')?;
    let path = String::from_utf8_lossy(&output[..first]).into_owned();
    let rest = &output[first + 1..];
    let second = rest.iter().position(|&b| b == b'\n')?;
    let body = &rest[second + 1..];
    let mut entries = match &rest[..second] {
        b"F" => parse_find(body),
        b"L" => parse_ls(&String::from_utf8_lossy(body)),
        _ => return None,
    };
    sort_entries(&mut entries);
    Some(Listing { path, entries })
}

/// Folders first, then by name, ignoring case.
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// `%y %Y %s %T@ %m %u %f %l`, each ended by a NUL.
fn parse_find(body: &[u8]) -> Vec<Entry> {
    let fields: Vec<&[u8]> = body.split(|&b| b == 0).collect();
    fields
        .chunks_exact(8)
        .filter_map(|f| {
            let text = |i: usize| String::from_utf8_lossy(f[i]).into_owned();
            let kind = match (f[0], f[1]) {
                (b"d", _) => EntryKind::Dir,
                (b"f", _) => EntryKind::File,
                (b"l", target) => EntryKind::Link { to_dir: target == b"d" },
                _ => EntryKind::Other,
            };
            let type_char = match f[0] {
                b"f" => '-',
                other => other.first().map_or('?', |&c| c as char),
            };
            let mode = u32::from_str_radix(&text(4), 8).ok()?;
            let name = text(6);
            if name.is_empty() || name == "." || name == ".." {
                return None;
            }
            Some(Entry {
                name,
                kind,
                size: text(2).parse().unwrap_or(0),
                mtime: text(3).split('.').next().and_then(|s| s.parse().ok()),
                mode: mode_string(type_char, mode),
                owner: text(5),
                target: matches!(kind, EntryKind::Link { .. }).then(|| text(7)),
            })
        })
        .collect()
}

/// `drwxr-xr-x` style text for a type character and permission bits.
fn mode_string(type_char: char, mode: u32) -> String {
    let mut out = String::with_capacity(10);
    out.push(type_char);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        out.push(if bits & 4 != 0 { 'r' } else { '-' });
        out.push(if bits & 2 != 0 { 'w' } else { '-' });
        out.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    out
}

/// `ls -la` lines, with the date as epoch seconds (GNU `--time-style=+%s`,
/// BSD `-D %s`) or as `Mon DD HH:MM` / `Mon DD YYYY`.
fn parse_ls(body: &str) -> Vec<Entry> {
    body.lines().filter_map(parse_ls_line).collect()
}

// Example:
// drwxr-xr-x  15 root root    4096 1789050999 ..
// lrwxrwxrwx   1 ion  ion        9 Oct  3 12:01 data -> /mnt/data
// crw-rw-rw-   1 root root   1,   3 Oct  3  2025 null
fn parse_ls_line(line: &str) -> Option<Entry> {
    let mut rest = line;
    let mut next = || {
        rest = rest.trim_start();
        let end = rest.find(' ').unwrap_or(rest.len());
        let (word, tail) = rest.split_at(end);
        rest = tail;
        (!word.is_empty()).then_some(word)
    };
    let mode = next()?;
    if mode.len() < 10 || mode == "total" {
        return None;
    }
    let _links = next()?;
    let owner = next()?.to_string();
    let _group = next()?;
    let mut size_word = next()?;
    // Devices show "major, minor" where the size goes.
    if size_word.ends_with(',') {
        next()?;
        size_word = "0";
    }
    let size = size_word.parse().unwrap_or(0);
    let date = next()?;
    let mtime = if date.bytes().all(|b| b.is_ascii_digit()) {
        date.parse().ok()
    } else {
        // Month, day, then time or year: shown as text-less (no epoch).
        next()?;
        next()?;
        None
    };
    let name_part = rest.strip_prefix(' ').unwrap_or(rest);
    let (name, target) = match mode.as_bytes()[0] {
        b'l' => match name_part.split_once(" -> ") {
            Some((name, target)) => (name.to_string(), Some(target.to_string())),
            None => (name_part.to_string(), None),
        },
        _ => (name_part.to_string(), None),
    };
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let kind = match mode.as_bytes()[0] {
        b'd' => EntryKind::Dir,
        b'-' => EntryKind::File,
        // Without find there's no cheap way to tell; a trailing slash in the
        // target is a good hint, and opening a file link just fails.
        b'l' => EntryKind::Link {
            to_dir: target.as_deref().is_some_and(|t| t.ends_with('/')),
        },
        _ => EntryKind::Other,
    };
    Some(Entry {
        name,
        kind,
        size,
        mtime,
        mode: mode[..10].to_string(),
        owner,
        target,
    })
}

//#endregion

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_sh() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote(""), "''");
        // Round-trip through a real shell, with every awkward character.
        let tricky = "a b'c\"d$e`f\\g\nh*?;&|";
        if let Ok(output) = Command::new("sh").args(["-c", &format!("printf %s {}", quote(tricky))]).output() {
            assert_eq!(String::from_utf8_lossy(&output.stdout), tricky);
        }
    }

    #[test]
    fn wrapped_script_is_shell_neutral() {
        let wrapped = wrap_script("echo 'hi' \"there\" $HOME `x` \\");
        let inner = wrapped.strip_prefix("exec sh -c 'eval \"$(printf %s ").unwrap();
        let b64 = inner.strip_suffix(" | base64 -d)\"'").unwrap();
        assert!(b64.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)));
    }

    #[test]
    fn splits_paths() {
        assert_eq!(split_path("/home/ion/file"), ("/home/ion", "file"));
        assert_eq!(split_path("/home/"), ("/", "home"));
        assert_eq!(split_path("/"), ("/", ""));
        assert_eq!(parent("/a/b"), "/a");
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(join("/home", "ion"), "/home/ion");
    }

    #[test]
    fn parses_find_output() {
        let mut out = b"/home/ion\nF\n".to_vec();
        out.extend_from_slice(b"f\0f\x001234\x001791566252.0954\x00644\0ion\0notes.txt\0\0");
        out.extend_from_slice(b"d\0d\x004096\x001791566252.5\x00755\0ion\0apps\0\0");
        out.extend_from_slice(b"l\0d\x009\x001791566252.5\x00777\0ion\0data\0/mnt/data\0");
        out.extend_from_slice(b"f\0f\x000\x001\x00600\0ion\0with\nnewline\0\0");
        let listing = parse_listing(&out).unwrap();
        assert_eq!(listing.path, "/home/ion");
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["apps", "data", "notes.txt", "with\nnewline"]);
        let notes = &listing.entries[2];
        assert_eq!((notes.size, notes.mtime, notes.mode.as_str()), (1234, Some(1791566252), "-rw-r--r--"));
        let data = &listing.entries[1];
        assert_eq!(data.kind, EntryKind::Link { to_dir: true });
        assert_eq!(data.target.as_deref(), Some("/mnt/data"));
        assert_eq!(listing.entries[0].mode, "drwxr-xr-x");
    }

    #[test]
    fn parses_ls_output() {
        let text = "/root\nL\ntotal 12\n\
drwxr-xr-x  15 root root    4096 1789050999 .\n\
-rw-r--r--   1 ion  ion      220 1633182620 .bash_logout\n\
lrwxrwxrwx   1 ion  ion        9 Oct  3 12:01 my data -> /mnt/data/\n\
crw-rw-rw-   1 root root   1,   3 Oct  3  2025 null\n\
-rwxr-xr-x   1 ion  ion    10240 Jan  1  2024 run me.sh\n";
        let listing = parse_listing(text.as_bytes()).unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        // The link to a folder sorts with the folders.
        assert_eq!(names, ["my data", ".bash_logout", "null", "run me.sh"]);
        let link = &listing.entries[0];
        assert_eq!(link.kind, EntryKind::Link { to_dir: true });
        assert!(link.is_dir());
        assert_eq!(listing.entries[1].mtime, Some(1633182620));
        assert_eq!(listing.entries[3].size, 10240);
        assert!(listing.entries[3].is_executable());
        assert_eq!(listing.entries[2].kind, EntryKind::Other);
    }

    #[test]
    fn lists_through_a_real_shell() {
        // `sh -c` stands in for ssh: the whole remote path runs locally.
        let dir = std::env::temp_dir().join(format!("ionterm-list-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub dir")).unwrap();
        std::fs::write(dir.join("it's a file"), b"hello").unwrap();
        let remote = Remote::with_argv(vec!["sh".into(), "-c".into()]);
        if let Ok(listing) = remote.list(Some(dir.to_str().unwrap())) {
            let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["sub dir", "it's a file"]);
            assert_eq!(listing.entries[1].size, 5);
            assert!(remote.existing(dir.to_str().unwrap(), &["it's a file".into(), "nope".into()]).unwrap() == ["it's a file"]);
            remote.rename(&join(dir.to_str().unwrap(), "it's a file"), &join(dir.to_str().unwrap(), "renamed")).unwrap();
            assert!(dir.join("renamed").exists());
            assert!(remote.mkdir(&join(dir.to_str().unwrap(), "sub dir")).is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
