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
pub use detect::DetectedSsh;

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
    pub alias: String,
    pub argv: Vec<String>,
}

impl Connection {
    /// Builds the `ssh` command for a host alias.
    pub fn new(alias: &str, host: Option<&SshHost>, extras: Option<&HostExtras>) -> Self {
        let mut argv = vec!["ssh".to_string()];
        if let Some(dir) = control_dir() {
            argv.extend([
                "-o".into(),
                "ControlMaster=auto".into(),
                "-o".into(),
                format!("ControlPath={}", dir.join("%C").display()),
                "-o".into(),
                // Keep the shared connection a while after the last tab closes,
                // so reopening is instant.
                "ControlPersist=300".into(),
            ]);
        }
        let integration = extras.and_then(|e| e.integration).unwrap_or(true) && !host.is_some_and(|h| h.remote_command);
        if integration {
            argv.extend(["-t".into(), alias.to_string(), bootstrap::remote_command()]);
        } else {
            argv.push(alias.to_string());
        }
        Self {
            alias: alias.to_string(),
            argv,
        }
    }
}

/// Where shared-connection sockets live: a private directory in the cache.
/// `None` on Windows, whose OpenSSH has no connection sharing.
fn control_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    let dir = dirs::cache_dir()?.join("ionterm").join("ssh");
    std::fs::create_dir_all(&dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Some(dir)
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
