//! First-class SSH: hosts from the user's ssh config, connections ionterm
//! opens itself, and recognising `ssh` sessions typed in a shell.
//!
//! Connections run the system's OpenSSH, so every setting, key and agent the
//! user already has applies. On Unix they share one master connection per
//! host, which makes extra tabs and file transfers start without logging in
//! again.

pub mod bootstrap;
pub mod config;
pub mod detect;
pub mod store;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

pub use config::{SshHost, load_hosts};
pub use detect::{DetectedSsh, SshArgs};

use crate::config::config_dir;
use crate::theme::ThemeColor;

/// ionterm's own per-host settings, in `~/.config/ionterm/hosts.toml`.
pub const HOSTS_FILE: &str = "hosts.toml";

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct HostExtras {
    /// Tab colour for this host.
    pub color: Option<ThemeColor>,
    /// A single (Nerd Font) character shown in the tab.
    pub icon: Option<String>,
    /// Group heading in the connection picker.
    pub group: Option<String>,
    /// Run the remote bootstrap (terminfo and directory reporting). Turn it
    /// off for hosts without a POSIX shell, like network gear.
    pub integration: Option<bool>,
}

impl HostExtras {
    pub fn icon_char(&self) -> Option<char> {
        self.icon.as_deref().and_then(|s| s.chars().next())
    }
}

#[derive(Debug, Default, Deserialize)]
struct ExtrasFile {
    #[serde(default)]
    hosts: HashMap<String, HostExtras>,
}

pub fn load_extras() -> HashMap<String, HostExtras> {
    let Some(path) = config_dir().map(|dir| dir.join(HOSTS_FILE)) else {
        return HashMap::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<ExtrasFile>(&text) {
            Ok(file) => file.hosts,
            Err(error) => {
                tracing::warn!("ignoring {}: {error}", path.display());
                HashMap::new()
            }
        },
        Err(_) => HashMap::new(),
    }
}

/// A connection ionterm opened, and how to open it again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    /// The host alias, or the destination given to `ionterm ssh`.
    pub alias: String,
    pub argv: Vec<String>,
    /// ssh options given to `ionterm ssh`, one flag per word. Empty for a
    /// config host.
    pub options: Vec<String>,
    /// Built by ionterm from a host alias (rather than copied from a typed
    /// `ssh`), so extra commands can share its connection: the file panel
    /// works only for these.
    pub managed: bool,
}

/// Flags of `ssh` that only concern the interactive session (tty,
/// forwarding, backgrounding), left out of the one-off commands that share
/// its connection.
const SESSION_OPTIONS: &str = "AaDfGLMNnRsTtWXxY";

impl Connection {
    /// Builds the `ssh` command for a host alias.
    pub fn new(alias: &str, host: Option<&SshHost>, extras: Option<&HostExtras>) -> Self {
        let args = SshArgs {
            destination: alias.to_string(),
            ..SshArgs::default()
        };
        Self::from_args(&args, host, extras)
    }

    /// Builds the `ssh` command for arguments given to `ionterm ssh`. `host`
    /// and `extras` apply when the destination is a config host alias.
    pub fn from_args(args: &SshArgs, host: Option<&SshHost>, extras: Option<&HostExtras>) -> Self {
        let mut argv = vec!["ssh".to_string()];
        argv.extend(control_options(true));
        argv.extend(args.options.iter().cloned());
        let integration = args.command.is_empty()
            && extras.and_then(|e| e.integration).unwrap_or(true)
            && !host.is_some_and(|h| h.remote_command);
        if integration {
            argv.extend(["-t".into(), args.destination.clone(), bootstrap::remote_command()]);
        } else {
            argv.push(args.destination.clone());
            argv.extend(args.command.iter().cloned());
        }
        Self {
            alias: args.destination.clone(),
            argv,
            options: args.options.clone(),
            managed: true,
        }
    }

    /// The start of an `ssh` command that runs one non-interactive command
    /// on this host over the shared connection; the remote command follows.
    /// `BatchMode` makes it fail rather than ask for a password where no
    /// shared connection exists (Windows, or the master went away).
    pub fn exec_argv(&self) -> Vec<String> {
        let mut argv = vec!["ssh".to_string()];
        argv.extend(control_options(false));
        argv.extend(self.connection_options());
        argv.extend(["-o".into(), "BatchMode=yes".into(), "-T".into(), "--".into(), self.alias.clone()]);
        argv
    }

    /// The given options that pick the connection (port, user, jump host,
    /// key, `-o`…), so one-off commands find the same shared connection.
    fn connection_options(&self) -> Vec<String> {
        let mut kept = Vec::new();
        let mut words = self.options.iter();
        while let Some(word) = words.next() {
            let flag = word.chars().nth(1).unwrap_or_default();
            let value = detect::OPTIONS_WITH_VALUES.contains(flag).then(|| words.next()).flatten();
            if !SESSION_OPTIONS.contains(flag) {
                kept.push(word.clone());
                kept.extend(value.cloned());
            }
        }
        kept
    }
}

/// The options that make `ssh` share one master connection per host.
/// `master` lets this `ssh` become the master when none is running; one-off
/// commands never do, because a backgrounded master keeps their stderr pipe
/// open, and reading it would wait for the master to exit.
fn control_options(master: bool) -> Vec<String> {
    let Some(dir) = control_dir() else { return Vec::new() };
    let mut options = vec![
        "-o".into(),
        format!("ControlPath={}", dir.join("%C").display()),
        "-o".into(),
        if master { "ControlMaster=auto" } else { "ControlMaster=no" }.into(),
    ];
    if master {
        // Keep the shared connection a while after the last tab closes,
        // so reopening is instant.
        options.extend(["-o".into(), "ControlPersist=300".into()]);
    }
    options
}

/// The longest directory a control socket fits in: sockets paths are cut
/// at 108 bytes, and ssh adds 41 for `/%C` plus a 17-byte temporary suffix
/// while it sets the master up.
const MAX_CONTROL_DIR: usize = 48;

/// Where shared-connection sockets live: a private directory in the cache,
/// or in the runtime directory (then `/tmp`) when the cache's path is too
/// long for a socket. `None` on Windows, whose OpenSSH has no connection
/// sharing.
fn control_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    let candidates = [
        dirs::cache_dir().map(|d| d.join("ionterm").join("ssh")),
        dirs::runtime_dir().map(|d| d.join("ionterm-ssh")),
        Some(std::env::temp_dir().join(format!("ionterm-ssh-{}", user_tag()))),
    ];
    let dir = candidates
        .into_iter()
        .flatten()
        .find(|d| d.as_os_str().len() <= MAX_CONTROL_DIR)?;
    std::fs::create_dir_all(&dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // In a shared /tmp someone else may have made it first.
        let mine = std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir() && Some(m.uid()) == own_uid());
        if !mine {
            return None;
        }
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Some(dir)
}

/// Tells users' directories in a shared `/tmp` apart.
fn user_tag() -> String {
    #[cfg(unix)]
    if let Some(uid) = own_uid() {
        return uid.to_string();
    }
    std::env::var("USER").unwrap_or_default()
}

/// This process's user.
#[cfg(unix)]
fn own_uid() -> Option<u32> {
    #[cfg(target_os = "linux")]
    return Some(rustix::process::getuid().as_raw());
    #[cfg(not(target_os = "linux"))]
    return None;
}

/// This machine's name, to tell local OSC 7 reports from remote ones.
pub fn local_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|s| s.trim().to_string())
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn shares_only_connection_options() {
        let args = detect::parse_args(&words("-tA -L 8080:localhost:80 -J jump me@box -p 2222 -o User=x")).unwrap();
        let connection = Connection::from_args(&args, None, None);
        assert_eq!(connection.alias, "me@box");
        assert_eq!(connection.connection_options(), words("-J jump -p 2222 -o User=x"));
        assert!(connection.exec_argv().ends_with(&words("-J jump -p 2222 -o User=x -o BatchMode=yes -T -- me@box")));
        assert!(connection.argv.ends_with(&["-t".into(), "me@box".into(), bootstrap::remote_command()]));

        let args = detect::parse_args(&words("box htop")).unwrap();
        assert!(Connection::from_args(&args, None, None).argv.ends_with(&words("box htop")));
    }
}
