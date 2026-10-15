//! Transfers through rsync, used when both ends have it.
//!
//! rsync resumes an interrupted file instead of starting over, and only
//! sends what changed when the destination already has a version of it.
//! It runs over the same shared ssh connection as everything else (`-e`).
//!
//! - `-s` hands paths over inside the rsync protocol, so no shell on either
//!   end splits or expands them.
//! - `--partial-dir` keeps an interrupted file out of sight in
//!   `.ionterm-partial/` next to its destination; the next try picks it up.
//!   A finished file is renamed into place, so the real name never holds a
//!   half-written file.
//! - Cancelling stops rsync with SIGTERM so it can save its partial file,
//!   which is then removed: a cancelled transfer leaves nothing behind.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{LazyLock, Mutex, OnceLock};

use super::remote::{self, Remote};
use super::transfer::{self, Control, Direction, Reporter, TransferSpec};

/// Where an interrupted file waits, next to its destination.
const PARTIAL_DIR: &str = ".ionterm-partial";

/// Whether transfers with this host can use rsync. Linux only: Windows has
/// no usual rsync, and cancelling relies on SIGTERM.
pub fn usable(remote: &Remote) -> bool {
    cfg!(target_os = "linux") && remote.ssh_options().is_some() && local_rsync() && remote_rsync(remote)
}

fn local_rsync() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| {
        Command::new("rsync")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Asks the host once whether it has rsync; the answer is kept per host.
fn remote_rsync(remote: &Remote) -> bool {
    static HOSTS: LazyLock<Mutex<HashMap<String, bool>>> = LazyLock::new(Mutex::default);
    let lock = || HOSTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(&found) = lock().get(&remote.alias) {
        return found;
    }
    match remote.run("if command -v rsync >/dev/null 2>&1; then echo yes; else echo no; fi") {
        Ok(output) => {
            let found = output.starts_with(b"yes");
            lock().insert(remote.alias.clone(), found);
            found
        }
        // Couldn't ask (the connection hiccuped): try again next time.
        Err(_) => false,
    }
}

/// `alias:path`, an rsync remote location.
fn remote_arg(remote: &Remote, path: &str, contents: bool) -> String {
    let slash = if contents && !path.ends_with('/') { "/" } else { "" };
    format!("{}:{path}{slash}", remote.alias)
}

/// A local path for rsync; a trailing slash means "the folder's contents".
fn local_arg(path: &Path, contents: bool) -> OsString {
    let mut arg = path.as_os_str().to_os_string();
    if contents {
        arg.push("/");
    }
    arg
}

/// rsync's `-e`: the ssh command, quoted the way rsync splits it.
fn ssh_command(options: &[String]) -> String {
    options
        .iter()
        .map(|word| {
            if word.is_empty() || word.contains(|c: char| c.is_whitespace() || "'\"\\".contains(c)) {
                format!("'{}'", word.replace('\'', "'\"'\"'"))
            } else {
                word.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn transfer(spec: &TransferSpec, control: &Control, reporter: &mut Reporter) -> Result<(), String> {
    let options = spec.remote.ssh_options().ok_or("Not an ssh connection.")?;
    let local = if spec.local_path.is_absolute() {
        spec.local_path.clone()
    } else {
        std::env::current_dir().map_err(|e| e.to_string())?.join(&spec.local_path)
    };
    let created = !local.exists();
    let (source, dest): (OsString, OsString) = match spec.direction {
        Direction::Download => {
            reporter.total = if spec.is_dir {
                spec.remote.folder_size(&spec.remote_path)
            } else {
                transfer::remote_size(&spec.remote, &spec.remote_path)
            };
            if let Some(parent) = local.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("Could not create {}: {e}", parent.display()))?;
            }
            (remote_arg(&spec.remote, &spec.remote_path, spec.is_dir).into(), local_arg(&local, spec.is_dir))
        }
        Direction::Upload => {
            reporter.total = Some(transfer::local_size(&local));
            (local_arg(&local, spec.is_dir), remote_arg(&spec.remote, &spec.remote_path, spec.is_dir).into())
        }
    };
    reporter.report(0, true);

    let mut command = Command::new("rsync");
    command
        .args(["-s", "--no-motd", "--info=progress2", "--no-human-readable", "--outbuf=N"])
        .arg(format!("--partial-dir={PARTIAL_DIR}"))
        // Links as links, permissions and modification times kept.
        .arg(if spec.is_dir { "-rlpt" } else { "-lpt" })
        .arg("-e")
        .arg(ssh_command(options))
        .arg(source)
        .arg(dest)
        // Plain numbers in the progress lines.
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if spec.is_dir {
        // List everything first, so the progress has a steady total.
        command.arg("--no-inc-recursive");
    }
    let child = command.spawn().map_err(|e| format!("Could not run rsync: {e}"))?;
    control.gentle.store(true, Ordering::Release);
    transfer::adopt(control, child)?;
    let mut stdout = transfer::take_stdout(control).ok_or("No output from rsync.")?;
    follow_progress(&mut stdout, reporter);
    let result = transfer::finish_child_with(control, rsync_error);
    if result.is_err() && control.is_cancelled() {
        clean_up_cancelled(spec, &local, created);
    }
    result
}

/// Reads rsync's progress lines until it exits.
///
/// Example (`--info=progress2 --no-human-readable`, lines end in `\r`):
/// `    1048576  34%  100.00MB/s    0:00:01 (xfr#1, to-chk=0/1)`
fn follow_progress(stdout: &mut impl Read, reporter: &mut Reporter) {
    let mut buffer = [0u8; 4096];
    let mut line = Vec::new();
    let mut last = 0;
    loop {
        let n = match stdout.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        for &byte in &buffer[..n] {
            if byte == b'\r' || byte == b'\n' {
                if let Some(bytes) = parse_progress(&line) {
                    last = bytes;
                    reporter.report(bytes, false);
                }
                line.clear();
            } else {
                line.push(byte);
            }
        }
    }
    reporter.report(last, true);
}

fn parse_progress(line: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(line).ok()?;
    let mut words = text.split_whitespace();
    let bytes = words.next()?;
    // Only progress lines have a percentage second.
    words.next().filter(|w| w.ends_with('%'))?;
    bytes.chars().filter(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// rsync's reason for failing. Its last line is a generic summary, so look
/// for the first specific one instead.
///
/// Example: `rsync: [sender] link_stat "/x" failed: No such file or directory (2)`
fn rsync_error(child: &mut Child, code: Option<i32>) -> String {
    let mut stderr = Vec::new();
    if let Some(pipe) = child.stderr.as_mut() {
        let _ = pipe.read_to_end(&mut stderr);
    }
    let text = String::from_utf8_lossy(&stderr);
    let specific = text.lines().map(str::trim).find_map(|line| {
        let rest = line.strip_prefix("rsync: ")?;
        let rest = ["[sender] ", "[receiver] ", "[generator] ", "[Receiver] "]
            .iter()
            .find_map(|p| rest.strip_prefix(p))
            .unwrap_or(rest);
        Some(rest.to_string())
    });
    let ssh = text.lines().map(str::trim).find(|line| line.starts_with("ssh: ")).map(String::from);
    match (specific, ssh) {
        (Some(message), _) | (None, Some(message)) => message,
        // 255 is ssh's own "couldn't connect".
        (None, None) if code == Some(255) => "Could not reach the server.".into(),
        (None, None) => remote::child_error(child, code),
    }
}

/// Removes what a cancelled transfer left: the partial file, and a folder
/// that only this download created.
fn clean_up_cancelled(spec: &TransferSpec, local: &Path, created: bool) {
    match (spec.direction, spec.is_dir) {
        (Direction::Download, false) => {
            let Some(dir) = local.parent() else { return };
            let partial = dir.join(PARTIAL_DIR);
            if let Some(name) = local.file_name() {
                let _ = std::fs::remove_file(partial.join(name));
            }
            let _ = std::fs::remove_dir(partial);
        }
        (Direction::Download, true) if created && !spec.merge => {
            let _ = std::fs::remove_dir_all(local);
        }
        (Direction::Download, true) => remove_local_partials(local),
        (Direction::Upload, is_dir) => {
            let path = remote::quote(&spec.remote_path);
            // The server's rsync saves its partial file as the connection
            // drops; give it a moment first.
            let script = if is_dir {
                format!("sleep 1; find {path} -type d -name {PARTIAL_DIR} -prune -exec rm -rf {{}} + 2>/dev/null; true")
            } else {
                let dir = remote::quote(&format!("{}/{PARTIAL_DIR}", remote::parent(&spec.remote_path)));
                let name = remote::quote(spec.remote_path.rsplit('/').next().unwrap_or_default());
                format!("sleep 1; rm -f -- {dir}/{name}; rmdir -- {dir} 2>/dev/null; true")
            };
            let _ = spec.remote.run(&script);
        }
    }
}

fn remove_local_partials(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if entry.file_name() == PARTIAL_DIR {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            remove_local_partials(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_progress_lines() {
        assert_eq!(parse_progress(b"     1048576  34%  100.00MB/s    0:00:01 (xfr#1, to-chk=0/1)"), Some(1_048_576));
        assert_eq!(parse_progress(b"  1,048,576  34%"), Some(1_048_576));
        assert_eq!(parse_progress(b"sending incremental file list"), None);
        assert_eq!(parse_progress(b""), None);
    }

    #[test]
    fn quotes_the_ssh_command() {
        let options = ["ssh", "-o", "ControlPath=/home/a b/.cache/%C", "-o", "BatchMode=yes"].map(String::from);
        assert_eq!(ssh_command(&options), "ssh -o 'ControlPath=/home/a b/.cache/%C' -o BatchMode=yes");
    }
}
