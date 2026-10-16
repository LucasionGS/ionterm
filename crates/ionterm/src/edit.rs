//! Editing a server's files in a local editor: `ionterm-edit <file>` on the
//! server, or "Edit" in the file panel. The file is copied into a private
//! local folder, opened in `$EDITOR` in a tab, and written back to the
//! server every time it's saved.
//!
//! Writing back checks first that nobody changed the file on the server
//! since it was read, then writes it in place (`cat > file`), so its owner,
//! mode and hard links stay as they were. Files you can't write can go
//! through `sudo`; its password is asked for once and kept in memory only
//! for as long as the file is open.
//!
//! The functions here block (each is one command over the tab's shared
//! connection), so the app runs them on threads.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::files::remote::{Remote, quote};
use crate::ssh::bootstrap::base64;

/// Prints a file's modification time and size, with GNU or BSD `stat`.
const STAT: &str = "st() { stat -c '%Y %s' -- \"$1\" 2>/dev/null || stat -f '%m %z' -- \"$1\"; }\n";

/// How the server's file is read and written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// As the user ionterm logged in as.
    User,
    /// Through `sudo`, with its password when it asks for one.
    Sudo(Option<Secret>),
}

/// A password, which debug output never shows.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// Background work on an edited file, reported to the event loop.
#[derive(Debug)]
pub enum Event {
    Probed(Result<Probe, String>),
    /// Whether `sudo` needs a password.
    SudoChecked(Result<bool, String>),
    /// A `sudo` password that worked, or why it didn't.
    PasswordChecked(Result<Secret, String>),
    Read(Result<(Vec<u8>, RemoteState), String>),
    /// The local copy changed on disk (the editor saved).
    Changed,
    /// A write back finished; it sent content with this fingerprint.
    Written(u64, Result<Written, String>),
    /// The server's current copy, to compare against.
    ServerCopy(Result<Vec<u8>, String>),
}

/// The server's file as last read or written: what a save expects to find.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteState {
    /// There's no such file yet; saving creates it.
    Missing,
    File { mtime: i64, size: u64 },
}

impl RemoteState {
    /// As the scripts print and compare it.
    fn token(&self) -> String {
        match self {
            Self::Missing => "missing".into(),
            Self::File { mtime, size } => format!("{mtime} {size}"),
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text == "missing" {
            return Some(Self::Missing);
        }
        let (mtime, size) = text.split_once(' ')?;
        Some(Self::File {
            mtime: mtime.trim().parse().ok()?,
            size: size.trim().parse().ok()?,
        })
    }
}

/// What a first look at the file found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    pub exists: bool,
    pub readable: bool,
    /// The file can be written, or created when it doesn't exist yet.
    pub writable: bool,
}

/// Looks at the file as the login user: whether it's there and whether
/// reading and writing it would need `sudo`.
pub fn probe(remote: &Remote, path: &str) -> Result<Probe, String> {
    let script = format!(
        "f={}\n\
         if [ -d \"$f\" ]; then echo \"$f is a folder.\" >&2; exit 4; fi\n\
         if [ -e \"$f\" ]; then\n\
           r=0; w=0; [ -r \"$f\" ] && r=1; [ -w \"$f\" ] && w=1; echo \"file $r $w\"\n\
         else\n\
           d=$(dirname -- \"$f\")\n\
           [ -d \"$d\" ] || {{ echo \"There's no folder $d.\" >&2; exit 4; }}\n\
           w=0; [ -w \"$d\" ] && w=1; echo \"missing 1 $w\"\n\
         fi\n",
        quote(path)
    );
    let output = remote.run(&script)?;
    let text = String::from_utf8_lossy(&output);
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.as_slice() {
        [kind, r, w] => Ok(Probe {
            exists: *kind == "file",
            readable: *r == "1",
            writable: *w == "1",
        }),
        _ => Err("The server's answer made no sense.".into()),
    }
}

/// Whether `sudo` works without a password (`Ok(false)`), needs one
/// (`Ok(true)`), or isn't there at all.
pub fn sudo_needs_password(remote: &Remote) -> Result<bool, String> {
    let output = remote.run("command -v sudo >/dev/null 2>&1 || { echo none; exit 0; }\nsudo -n true 2>/dev/null && echo ok || echo password\n")?;
    match String::from_utf8_lossy(&output).trim() {
        "ok" => Ok(false),
        "password" => Ok(true),
        _ => Err("There's no sudo on the server.".into()),
    }
}

/// Checks a `sudo` password.
pub fn check_sudo_password(remote: &Remote, password: Secret) -> Result<Secret, String> {
    let access = Access::Sudo(Some(password));
    run(remote, "true\n", &access, Vec::new()).map_err(|_| "That password didn't work.".to_string())?;
    match access {
        Access::Sudo(Some(password)) => Ok(password),
        _ => unreachable!("built above"),
    }
}

/// Reads the file and its state; a missing file reads as empty.
pub fn read(remote: &Remote, path: &str, access: &Access) -> Result<(Vec<u8>, RemoteState), String> {
    let script = format!("{STAT}f={}\nif [ -e \"$f\" ]; then st \"$f\"; exec cat -- \"$f\"; else echo missing; fi\n", quote(path));
    let output = run(remote, &script, access, Vec::new())?;
    let newline = output.iter().position(|&b| b == b'\n').ok_or("The server's answer made no sense.")?;
    let state = RemoteState::parse(&String::from_utf8_lossy(&output[..newline])).ok_or("The server's answer made no sense.")?;
    Ok((output[newline + 1..].to_vec(), state))
}

/// What writing the file back did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Written {
    Saved(RemoteState),
    /// Someone changed the file on the server since; nothing was written.
    Conflict(RemoteState),
}

/// Writes `content` over the file, unless it changed on the server since
/// it was `expected` that way (`None` writes regardless). The content goes
/// to a temporary file first and is only copied over the real one once all
/// of it arrived.
pub fn write(remote: &Remote, path: &str, content: Vec<u8>, expected: Option<&RemoteState>, access: &Access) -> Result<Written, String> {
    let expected = expected.map(RemoteState::token).unwrap_or_default();
    let script = format!(
        "{STAT}f={path}\n\
         cur=$(st \"$f\" 2>/dev/null) || cur=missing\n\
         if [ -n {expected} ] && [ \"$cur\" != {expected} ]; then printf 'conflict %s\\n' \"$cur\"; exit 0; fi\n\
         t=$(mktemp) || exit 1\n\
         trap 'rm -f -- \"$t\"' EXIT\n\
         cat > \"$t\" || exit 1\n\
         [ \"$(wc -c < \"$t\" | tr -d ' ')\" = {size} ] || {{ echo 'The file arrived cut short; the server copy is unchanged.' >&2; exit 1; }}\n\
         cat -- \"$t\" > \"$f\" || exit 1\n\
         printf 'saved %s\\n' \"$(st \"$f\")\"\n",
        path = quote(path),
        expected = quote(&expected),
        size = content.len(),
    );
    let output = run(remote, &script, access, content)?;
    let text = String::from_utf8_lossy(&output);
    let line = text.lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default();
    let bad = || "The server's answer made no sense.".to_string();
    match line.split_once(' ') {
        Some(("saved", state)) => RemoteState::parse(state).map(Written::Saved).ok_or_else(bad),
        Some(("conflict", state)) => RemoteState::parse(state).map(Written::Conflict).ok_or_else(bad),
        _ => Err(bad()),
    }
}

/// Runs a script on the server as `access` says. With a `sudo` password,
/// stdin starts with it (`sudo -S` reads it one byte at a time, so the rest
/// of stdin reaches the script).
fn run(remote: &Remote, script: &str, access: &Access, input: Vec<u8>) -> Result<Vec<u8>, String> {
    let as_root = |flags: &str| format!("exec sudo {flags} sh -c \"$(printf %s {} | base64 -d)\"\n", base64(script.as_bytes()));
    match access {
        Access::User => remote.run_with_input(script, input),
        Access::Sudo(None) => remote.run_with_input(&as_root("-n"), input),
        Access::Sudo(Some(Secret(password))) => {
            let mut stdin = Vec::with_capacity(password.len() + 1 + input.len());
            stdin.extend_from_slice(password.as_bytes());
            stdin.push(b'\n');
            stdin.extend(input);
            remote.run_with_input(&as_root("-S -p ''"), stdin)
        }
    }
}

/// A fingerprint of a file's content, to tell a real change from a save
/// that wrote the same bytes (or a watcher that fired twice).
pub fn fingerprint(content: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// A private folder for one edited file's local copy.
pub fn local_dir(id: u64) -> std::io::Result<PathBuf> {
    let base = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir).join("ionterm").join("edit");
    let dir = base.join(format!("{}-{id}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Removes the local copies this ionterm made (it's quitting).
pub fn remove_local_dirs() {
    let base = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir).join("ionterm").join("edit");
    let Ok(entries) = std::fs::read_dir(base) else { return };
    let prefix = format!("{}-", std::process::id());
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// The name a remote file gets locally: its own, so the editor shows it
/// (and picks its syntax from it).
pub fn local_name(remote_path: &str) -> String {
    let name = remote_path.rsplit('/').next().unwrap_or_default();
    if name.is_empty() || name == "." || name == ".." { "file".into() } else { name.to_string() }
}

/// Whether a file panel entry is likely text worth opening in an editor
/// rather than downloading: small, not a program, no binary extension.
pub fn looks_editable(name: &str, size: u64, executable: bool) -> bool {
    const BINARY: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tiff", "pdf", "zip", "gz", "tgz", "xz", "zst", "bz2", "7z", "rar", "tar",
        "iso", "img", "deb", "rpm", "so", "a", "o", "bin", "exe", "dll", "class", "jar", "mp3", "mp4", "mkv", "avi", "mov", "flac",
        "ogg", "wav", "woff", "woff2", "ttf", "otf", "sqlite", "db", "pyc",
    ];
    let extension = Path::new(name).extension().map(|e| e.to_string_lossy().to_lowercase());
    size <= 4 << 20 && !executable && !extension.is_some_and(|e| BINARY.contains(&e.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A "server" that is just `sh -c` here, in a scratch folder.
    fn local() -> (Remote, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ionterm-edit-test-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        (Remote::with_argv(vec!["sh".into(), "-c".into()]), dir)
    }

    fn rand_suffix() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
    }

    #[test]
    fn states_round_trip() {
        for state in [RemoteState::Missing, RemoteState::File { mtime: 1_700_000_000, size: 42 }] {
            assert_eq!(RemoteState::parse(&state.token()), Some(state));
        }
        assert_eq!(RemoteState::parse("nonsense"), None);
    }

    #[test]
    fn reads_and_writes_in_place() {
        let (remote, dir) = local();
        let file = dir.join("app's.conf");
        std::fs::write(&file, "old\n").unwrap();
        let path = file.to_string_lossy().into_owned();

        let probe = probe(&remote, &path).unwrap();
        assert_eq!(probe, Probe { exists: true, readable: true, writable: true });
        let (content, state) = read(&remote, &path, &Access::User).unwrap();
        assert_eq!(content, b"old\n");

        // In place: a hard link sees the new content too.
        let link = dir.join("link");
        std::fs::hard_link(&file, &link).unwrap();
        let saved = write(&remote, &path, b"new\ncontent\n".to_vec(), Some(&state), &Access::User).unwrap();
        let Written::Saved(new_state) = saved else { panic!("not saved: {saved:?}") };
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "new\ncontent\n");
        assert!(matches!(new_state, RemoteState::File { size: 12, .. }));

        // Changed behind our back: a conflict, and nothing written.
        std::fs::write(&file, "someone else\n").unwrap();
        let stale = RemoteState::File { mtime: 1, size: 12 };
        assert!(matches!(write(&remote, &path, b"mine\n".to_vec(), Some(&stale), &Access::User), Ok(Written::Conflict(_))));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "someone else\n");
        // Unless told to write anyway.
        assert!(matches!(write(&remote, &path, b"mine\n".to_vec(), None, &Access::User), Ok(Written::Saved(_))));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn new_files_and_folders() {
        let (remote, dir) = local();
        let path = dir.join("new.txt").to_string_lossy().into_owned();
        assert_eq!(probe(&remote, &path).unwrap(), Probe { exists: false, readable: true, writable: true });
        let (content, state) = read(&remote, &path, &Access::User).unwrap();
        assert!(content.is_empty());
        assert_eq!(state, RemoteState::Missing);
        assert!(matches!(write(&remote, &path, b"hi\n".to_vec(), Some(&state), &Access::User), Ok(Written::Saved(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hi\n");
        assert!(probe(&remote, &dir.to_string_lossy()).unwrap_err().contains("folder"));
        assert!(probe(&remote, "/nonexistent-ionterm/x").unwrap_err().contains("no folder"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    // The stand-in sudo is a shebang script found through a `:`-separated
    // PATH, which only works where the shell is a real Unix one.
    #[cfg(unix)]
    #[test]
    fn goes_through_sudo() {
        let (_, dir) = local();
        // A stand-in sudo: -n says a password is needed, -S takes "pw".
        let fake = dir.join("sudo");
        std::fs::write(
            &fake,
            "#!/bin/sh\nn=0; s=0\nwhile [ $# -gt 0 ]; do case $1 in -n) n=1; shift;; -S) s=1; shift;; -p) shift 2;; *) break;; esac; done\n\
             [ $n = 1 ] && { echo 'sudo: a password is required' >&2; exit 1; }\n\
             if [ $s = 1 ]; then IFS= read -r pw; [ \"$pw\" = pw ] || { echo 'Sorry, try again.' >&2; exit 1; }; fi\nexec \"$@\"\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path_var = format!("PATH={}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
        let remote = Remote::with_argv(vec!["env".into(), path_var, "sh".into(), "-c".into()]);

        assert_eq!(sudo_needs_password(&remote), Ok(true));
        assert!(check_sudo_password(&remote, Secret("nope".into())).is_err());
        let password = check_sudo_password(&remote, Secret("pw".into())).unwrap();
        let access = Access::Sudo(Some(password));

        // The password goes first on stdin, the content after it.
        let path = dir.join("root.conf").to_string_lossy().into_owned();
        std::fs::write(&path, "a\n").unwrap();
        let (content, state) = read(&remote, &path, &access).unwrap();
        assert_eq!(content, b"a\n");
        assert!(matches!(write(&remote, &path, b"b\n".to_vec(), Some(&state), &access), Ok(Written::Saved(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "b\n");
        assert!(read(&remote, &path, &Access::Sudo(None)).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn guesses_what_to_edit() {
        assert!(looks_editable("nginx.conf", 2000, false));
        assert!(looks_editable("Makefile", 2000, false));
        assert!(!looks_editable("photo.JPG", 2000, false));
        assert!(!looks_editable("server", 2000, true));
        assert!(!looks_editable("huge.log", 64 << 20, false));
    }
}
