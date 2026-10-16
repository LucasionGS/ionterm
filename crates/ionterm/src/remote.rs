//! Remote control: `ionterm @ <command>` talks to a running ionterm over a
//! Unix socket in the runtime directory. Programs in a pane find their own
//! window through `IONTERM_SOCKET` and their pane through `IONTERM_PANE`.
//!
//! The protocol is one request per connection: the client writes its pane
//! (or nothing) and the command's arguments, each ended by a NUL, then
//! closes its side; the window answers with an exit code on the first line
//! and the output after it.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::mpsc;

/// Commands, for `ionterm @ help` and errors.
pub const USAGE: &str = "\
Usage: ionterm @ <command> [options]

Commands:
  ls                              List windows, tabs and panes (window, tab, pane, active, cwd, title)
  send-text [-p PANE] <text...>   Type text into a pane (\\n, \\r, \\t, \\e and \\\\ escapes)
  get-text [-p PANE] [-a]         Print a pane's screen, or with -a its whole scrollback
  new-tab [--cwd DIR] [--title T] [command...]
                                  Open a tab, running a command or the shell; prints its pane
  new-window [--cwd DIR] [--title T] [--host HOST | --ssh SSH-ARGS... | command...]
                                  Open a window with a shell, a host, ssh arguments or a command;
                                  prints its pane
  host <host>                     Open a tab connected to an ssh config host; prints its pane
  ssh [ssh options] <destination> [command]
                                  Open a tab connected with ssh arguments; prints its pane
  split [-p PANE] [--right|--down] [--cwd DIR] [command...]
                                  Split a pane; prints the new pane
  move-tab [-p PANE] (--new-window | --window N)
                                  Move a pane's tab to a new window, or window N of \"ls\"
  move-pane [-p PANE] (--new-tab | --new-window | --tab N [--window N])
                                  Move a pane to its own tab or window, or into tab N as a split
  focus -p PANE                   Show a pane and give it the keyboard
  close [-p PANE]                 Close a pane
  set-title [-p PANE] [title]     Name a pane's tab (no title goes back to automatic)
  action [-p PANE] <name>         Run a keybinding action (new_tab, toggle_zoom, search…)
  version                         Print ionterm's version and the path of its program

PANE defaults to the pane the command runs in, or else the active one.";

/// Exit codes: 0 done, 1 failed, 2 bad arguments, 3 no ionterm to talk to.
pub const BAD_ARGUMENTS: u8 = 2;
const NOT_RUNNING: u8 = 3;

/// A command from a client, answered once from the event loop.
pub struct Request {
    /// The pane the client runs in, if it runs in one of ours.
    pub pane: Option<u64>,
    pub args: Vec<String>,
    reply: mpsc::Sender<(u8, String)>,
}

impl Request {
    pub fn reply(self, code: u8, text: impl Into<String>) {
        let _ = self.reply.send((code, text.into()));
    }
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request").field("pane", &self.pane).field("args", &self.args).finish()
    }
}

static SOCKET: OnceLock<PathBuf> = OnceLock::new();

/// The socket this process listens on, once it does.
pub fn socket_path() -> Option<&'static PathBuf> {
    SOCKET.get()
}

#[cfg(unix)]
fn socket_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// Starts listening; every request goes to `deliver` (which wakes the event
/// loop). Failing to listen only costs remote control, so it's logged.
#[cfg(unix)]
pub fn listen(deliver: impl Fn(Request) -> bool + Send + 'static) {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    remove_stale_sockets();
    let path = socket_dir().join(format!("ionterm-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!("remote control is off: can't listen on {}: {error}", path.display());
            return;
        }
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let _ = SOCKET.set(path);
    let spawned = std::thread::Builder::new().name("remote-control".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            if !serve(stream, &deliver) {
                break;
            }
        }
    });
    if let Err(error) = spawned {
        tracing::warn!("remote control is off: {error}");
    }
}

#[cfg(not(unix))]
pub fn listen(_deliver: impl Fn(Request) -> bool + Send + 'static) {}

/// Removes sockets left by ionterms that were killed rather than closed:
/// nothing answers on them any more.
#[cfg(unix)]
fn remove_stale_sockets() {
    let Ok(entries) = std::fs::read_dir(socket_dir()) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("ionterm-")
            && name.ends_with(".sock")
            && std::os::unix::net::UnixStream::connect(entry.path()).is_err()
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Answers one connection. Returns false once the window is gone.
#[cfg(unix)]
fn serve(mut stream: std::os::unix::net::UnixStream, deliver: &impl Fn(Request) -> bool) -> bool {
    use std::io::{Read, Write};
    use std::time::Duration;

    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut bytes = Vec::new();
    // 1 MiB is plenty for any command; more is someone else's stream.
    // Nothing sent: another ionterm checking whether this one is alive.
    if (&mut stream).take(1 << 20).read_to_end(&mut bytes).is_err() || bytes.is_empty() {
        return true;
    }
    let mut fields = bytes.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f).into_owned());
    let pane = fields.next().and_then(|p| p.parse().ok());
    let mut args: Vec<String> = fields.collect();
    // The NUL after the last argument leaves an empty field behind.
    if args.last().is_some_and(String::is_empty) {
        args.pop();
    }
    let (reply, answer) = mpsc::channel();
    if !deliver(Request { pane, args, reply }) {
        return false;
    }
    let (code, text) = answer
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or((1, "ionterm didn't answer in time.".into()));
    let _ = write!(stream, "{code}\n{text}");
    true
}

/// Removes the socket when the window closes.
pub fn remove_socket() {
    if let Some(path) = SOCKET.get() {
        let _ = std::fs::remove_file(path);
    }
}

/// `ionterm @ …`: sends the command and prints the answer.
pub fn client(args: Vec<String>) -> ExitCode {
    if args.is_empty() || matches!(args[0].as_str(), "help" | "-h" | "--help") {
        println!("{USAGE}");
        return if args.is_empty() { ExitCode::from(BAD_ARGUMENTS) } else { ExitCode::SUCCESS };
    }
    let (code, text) = match send(&args) {
        Some(Ok(answer)) => answer,
        Some(Err(error)) => {
            eprintln!("Lost the connection to ionterm: {error}");
            return ExitCode::FAILURE;
        }
        None if cfg!(unix) => {
            eprintln!("No ionterm to talk to. Run this inside ionterm, or set IONTERM_SOCKET to its socket.");
            return ExitCode::from(NOT_RUNNING);
        }
        None => {
            eprintln!("Remote control isn't available on Windows yet.");
            return ExitCode::from(NOT_RUNNING);
        }
    };
    if code == 0 {
        print!("{text}");
    } else {
        eprint!("{text}");
    }
    if !text.is_empty() && !text.ends_with('\n') {
        if code == 0 { println!() } else { eprintln!() }
    }
    ExitCode::from(code)
}

/// Sends a command to a running ionterm and returns its exit code and
/// output. `None` when no ionterm answers.
pub fn send(args: &[String]) -> Option<std::io::Result<(u8, String)>> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;

        let stream = match std::env::var_os("IONTERM_SOCKET") {
            Some(path) => UnixStream::connect(path).ok()?,
            None => sockets_newest_first().into_iter().find_map(|path| UnixStream::connect(path).ok())?,
        };
        Some(exchange(stream, args, None))
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        None
    }
}

/// Sends a command to the ionterm listening on `socket`.
#[cfg(unix)]
pub fn send_to(socket: &std::path::Path, args: &[String]) -> std::io::Result<(u8, String)> {
    exchange(std::os::unix::net::UnixStream::connect(socket)?, args, None)
}

/// The socket of a running ionterm that is this same program, for
/// `single_instance`: a development build never hands its windows to the
/// installed one, and an older ionterm (no `version` command) gets none.
pub fn find_instance() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let me = std::env::current_exe().and_then(|p| p.canonicalize()).ok()?;
        let candidates = std::env::var_os("IONTERM_SOCKET").map(PathBuf::from).into_iter().chain(sockets_newest_first());
        for path in candidates {
            let Ok(stream) = UnixStream::connect(&path) else { continue };
            let Ok((0, text)) = exchange(stream, &["version".to_string()], Some(Duration::from_secs(2))) else { continue };
            let theirs = text.lines().nth(1).and_then(|exe| std::fs::canonicalize(exe).ok());
            if theirs.as_ref() == Some(&me) {
                return Some(path);
            }
        }
        None
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Every ionterm socket in the runtime directory, the newest first.
#[cfg(unix)]
fn sockets_newest_first() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(socket_dir()) else { return Vec::new() };
    let mut sockets: Vec<_> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("ionterm-") && name.ends_with(".sock")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    sockets.sort_by_key(|s| std::cmp::Reverse(s.0));
    sockets.into_iter().map(|(_, path)| path).collect()
}

/// One request and its answer: see the protocol at the top.
#[cfg(unix)]
fn exchange(
    mut stream: std::os::unix::net::UnixStream,
    args: &[String],
    timeout: Option<std::time::Duration>,
) -> std::io::Result<(u8, String)> {
    use std::io::{Read, Write};

    stream.set_read_timeout(timeout)?;
    let pane = std::env::var("IONTERM_PANE").unwrap_or_default();
    let mut request = Vec::new();
    for field in std::iter::once(pane.as_str()).chain(args.iter().map(String::as_str)) {
        request.extend_from_slice(field.as_bytes());
        request.push(0);
    }
    let mut answer = String::new();
    stream.write_all(&request)?;
    stream.shutdown(std::net::Shutdown::Write)?;
    stream.read_to_string(&mut answer)?;
    let (code, text) = answer.split_once('\n').unwrap_or((answer.as_str(), ""));
    Ok((code.trim().parse().unwrap_or(1), text.to_string()))
}

/// Turns `\n`, `\r`, `\t`, `\e` and `\\` into the characters they name;
/// other backslashes stay as they are.
pub fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A command's options and the words after them. `flags` take no value;
/// any other option takes the next word. Options stop at the first word
/// that isn't one, or after `--`.
pub struct Parsed {
    pub options: Vec<(String, String)>,
    pub words: Vec<String>,
}

impl Parsed {
    pub fn parse(args: &[String], flags: &[&str], valued: &[&str]) -> Result<Self, String> {
        let mut options = Vec::new();
        let mut iter = args.iter();
        let mut words = Vec::new();
        while let Some(arg) = iter.next() {
            if arg == "--" {
                break;
            }
            if !arg.starts_with('-') || arg == "-" {
                words.push(arg.clone());
                break;
            }
            if flags.contains(&arg.as_str()) {
                options.push((arg.clone(), String::new()));
            } else if valued.contains(&arg.as_str()) {
                let Some(value) = iter.next() else {
                    return Err(format!("{arg} needs a value."));
                };
                options.push((arg.clone(), value.clone()));
            } else {
                return Err(format!("Unknown option: {arg}."));
            }
        }
        words.extend(iter.cloned());
        Ok(Self { options, words })
    }

    pub fn has(&self, names: &[&str]) -> bool {
        self.options.iter().any(|(name, _)| names.contains(&name.as_str()))
    }

    pub fn value(&self, names: &[&str]) -> Option<&str> {
        self.options.iter().rev().find(|(name, _)| names.contains(&name.as_str())).map(|(_, v)| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn parses_options_then_words() {
        let parsed = Parsed::parse(&strings(&["-p", "3", "--right", "htop", "-d", "5"]), &["--right"], &["-p"]).unwrap();
        assert_eq!(parsed.value(&["-p", "--pane"]), Some("3"));
        assert!(parsed.has(&["--right"]));
        assert_eq!(parsed.words, strings(&["htop", "-d", "5"]));
        assert!(Parsed::parse(&strings(&["--nope"]), &[], &[]).is_err());
        assert!(Parsed::parse(&strings(&["-p"]), &[], &["-p"]).is_err());
        let parsed = Parsed::parse(&strings(&["--", "-x"]), &[], &[]).unwrap();
        assert_eq!(parsed.words, strings(&["-x"]));
    }

    #[test]
    fn unescapes() {
        assert_eq!(unescape(r"ls\n"), "ls\n");
        assert_eq!(unescape(r"a\\b\q"), "a\\b\\q");
        assert_eq!(unescape(r"\e[A"), "\x1b[A");
    }
}
