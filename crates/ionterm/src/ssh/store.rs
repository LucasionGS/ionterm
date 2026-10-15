//! Saving the connections ionterm manages.
//!
//! They live in `~/.ssh/config.d/ionterm`, which `~/.ssh/config` pulls in
//! with one `Include` line, so plain `ssh <alias>` knows them too and the
//! user's own config is never rewritten. Colour, icon and group go to
//! `~/.config/ionterm/hosts.toml`, edited in place so comments survive.

use std::path::{Path, PathBuf};

use ionterm_vt::Rgb;

use super::config::{SshHost, ssh_dir};
use super::HOSTS_FILE;
use crate::config::config_dir;

/// The managed file, relative to `~/.ssh`.
pub const MANAGED_FILE: &str = "config.d/ionterm";
const INCLUDE_LINE: &str = "Include config.d/ionterm";
/// Kept next to `~/.ssh/config` the first time ionterm adds its `Include`.
const BACKUP_FILE: &str = "config.ionterm-backup";

/// The connection settings the editor edits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostForm {
    pub alias: String,
    pub hostname: String,
    pub user: String,
    pub port: Option<u16>,
    pub identity_file: String,
    pub proxy_jump: String,
}

impl HostForm {
    pub fn from_host(host: &SshHost) -> Self {
        Self {
            alias: host.alias.clone(),
            hostname: host.hostname.clone().unwrap_or_default(),
            user: host.user.clone().unwrap_or_default(),
            port: host.port,
            identity_file: host.identity_file.clone().unwrap_or_default(),
            proxy_jump: host.proxy_jump.clone().unwrap_or_default(),
        }
    }

    /// The `Key value` lines this form writes, in order.
    fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!("    HostName {}", self.hostname)];
        if !self.user.is_empty() {
            lines.push(format!("    User {}", self.user));
        }
        if let Some(port) = self.port {
            lines.push(format!("    Port {port}"));
        }
        if !self.identity_file.is_empty() {
            lines.push(format!("    IdentityFile {}", quote(&self.identity_file)));
        }
        if !self.proxy_jump.is_empty() {
            lines.push(format!("    ProxyJump {}", self.proxy_jump));
        }
        lines
    }
}

/// Keywords the form owns; any other line in a managed host is kept as is.
const FORM_KEYS: [&str; 5] = ["hostname", "user", "port", "identityfile", "proxyjump"];

fn quote(value: &str) -> String {
    if value.contains(char::is_whitespace) { format!("\"{value}\"") } else { value.to_string() }
}

pub fn managed_path() -> Option<PathBuf> {
    ssh_dir().map(|dir| dir.join(MANAGED_FILE))
}

/// Whether a host was defined in the managed file (and so can be edited).
pub fn is_managed(host: &SshHost) -> bool {
    managed_path().is_some_and(|path| host.source == path)
}

/// The managed file as a header (comments before any host) and host blocks.
#[derive(Debug, Default, PartialEq)]
struct ManagedFile {
    header: Vec<String>,
    /// (alias, body lines) in file order.
    blocks: Vec<(String, Vec<String>)>,
}

fn parse_managed(text: &str) -> ManagedFile {
    let mut file = ManagedFile::default();
    for line in text.lines() {
        let trimmed = line.trim();
        let keyword = trimmed.split(|c: char| c.is_whitespace() || c == '=').next().unwrap_or("");
        if keyword.eq_ignore_ascii_case("host") {
            let alias = trimmed[keyword.len()..].trim_start_matches(|c: char| c.is_whitespace() || c == '=');
            file.blocks.push((alias.trim().to_string(), Vec::new()));
        } else if let Some((_, body)) = file.blocks.last_mut() {
            body.push(line.to_string());
        } else {
            file.header.push(line.to_string());
        }
    }
    file
}

fn render_managed(file: &ManagedFile) -> String {
    let mut out = String::new();
    let header = if file.header.iter().all(|l| l.trim().is_empty()) {
        vec![
            "# Connections managed by ionterm (ctrl+shift+s, then ctrl+e to edit).".to_string(),
            "# Editing by hand is fine: ionterm keeps lines it doesn't manage.".to_string(),
        ]
    } else {
        let mut lines: Vec<String> = file.header.iter().map(|l| l.trim_end().to_string()).collect();
        // Blocks are separated by one blank line; trailing ones in the header
        // would otherwise pile up with every save.
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines
    };
    for line in &header {
        out.push_str(line);
        out.push('\n');
    }
    for (alias, body) in &file.blocks {
        out.push('\n');
        out.push_str(&format!("Host {alias}\n"));
        for line in body.iter().filter(|l| !l.trim().is_empty()) {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Replaces (or adds) one host's block, keeping lines the form doesn't own.
fn upsert(file: &mut ManagedFile, form: &HostForm, previous_alias: Option<&str>) {
    let lookup = previous_alias.unwrap_or(&form.alias);
    let kept: Vec<String> = file
        .blocks
        .iter()
        .find(|(alias, _)| alias == lookup)
        .map(|(_, body)| {
            body.iter()
                .filter(|line| {
                    let key = line.trim().split(|c: char| c.is_whitespace() || c == '=').next().unwrap_or("");
                    !FORM_KEYS.contains(&key.to_ascii_lowercase().as_str())
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut body = form.lines();
    body.extend(kept);
    match file.blocks.iter_mut().find(|(alias, _)| alias == lookup) {
        Some(block) => *block = (form.alias.clone(), body),
        None => file.blocks.push((form.alias.clone(), body)),
    }
}

/// Saves a connection to the managed file and makes sure `~/.ssh/config`
/// includes it. `previous_alias` is the host being edited, if any.
pub fn save_host(form: &HostForm, previous_alias: Option<&str>) -> std::io::Result<()> {
    let path = managed_path().ok_or_else(|| std::io::Error::other("no home directory"))?;
    let mut file = parse_managed(&std::fs::read_to_string(&path).unwrap_or_default());
    upsert(&mut file, form, previous_alias);
    write_private(&path, &render_managed(&file))?;
    ensure_include()
}

pub fn delete_host(alias: &str) -> std::io::Result<()> {
    let path = managed_path().ok_or_else(|| std::io::Error::other("no home directory"))?;
    let mut file = parse_managed(&std::fs::read_to_string(&path).unwrap_or_default());
    file.blocks.retain(|(a, _)| a != alias);
    write_private(&path, &render_managed(&file))
}

/// Adds `Include config.d/ionterm` at the top of `~/.ssh/config` unless an
/// existing top-level `Include` already covers the managed file. `Include`
/// only applies everywhere before the first `Host`, so it goes first.
pub fn ensure_include() -> std::io::Result<()> {
    let dir = ssh_dir().ok_or_else(|| std::io::Error::other("no home directory"))?;
    let config = dir.join("config");
    let text = std::fs::read_to_string(&config).unwrap_or_default();
    if include_covers(&text) {
        return Ok(());
    }
    let backup = dir.join(BACKUP_FILE);
    if config.exists() && !backup.exists() {
        std::fs::copy(&config, &backup)?;
    }
    let updated = if text.is_empty() {
        format!("{INCLUDE_LINE}\n")
    } else {
        format!("{INCLUDE_LINE}\n\n{text}")
    };
    write_private(&config, &updated)
}

fn include_covers(config: &str) -> bool {
    for line in config.lines() {
        let trimmed = line.trim();
        let mut words = trimmed.split(|c: char| c.is_whitespace() || c == '=').filter(|w| !w.is_empty());
        let Some(keyword) = words.next() else { continue };
        if keyword.eq_ignore_ascii_case("host") || keyword.eq_ignore_ascii_case("match") {
            return false;
        }
        if keyword.eq_ignore_ascii_case("include")
            && words.any(|pattern| {
                let pattern = pattern.trim_start_matches("~/.ssh/");
                super::config::wildcard(pattern, MANAGED_FILE)
            })
        {
            return true;
        }
    }
    false
}

/// Writes a file only the user can read, replacing it atomically.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if parent.ends_with("config.d") {
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    let temp = path.with_extension("ionterm-tmp");
    std::fs::write(&temp, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temp, path)
}

/// Saves a host's colour, icon and group to `hosts.toml`, moving them when
/// the host was renamed. Other settings and comments are left alone.
pub fn save_extras(
    alias: &str,
    previous_alias: Option<&str>,
    color: Option<Rgb>,
    icon: &str,
    group: &str,
) -> std::io::Result<()> {
    let dir = config_dir().ok_or_else(|| std::io::Error::other("no config directory"))?;
    let path = dir.join(HOSTS_FILE);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| std::io::Error::other(e.to_string()))?;

    let hosts = doc
        .entry("hosts")
        .or_insert_with(|| {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            toml_edit::Item::Table(table)
        })
        .as_table_mut()
        .ok_or_else(|| std::io::Error::other("`hosts` in hosts.toml is not a table"))?;
    // A rename carries over settings the editor doesn't show (like `integration`).
    let mut entry = previous_alias
        .filter(|old| *old != alias)
        .and_then(|old| hosts.remove(old))
        .or_else(|| hosts.remove(alias))
        .and_then(|item| item.into_table().ok())
        .unwrap_or_default();
    let mut set = |key: &str, value: Option<String>| match value {
        Some(value) => {
            entry.insert(key, toml_edit::value(value));
        }
        None => {
            entry.remove(key);
        }
    };
    set("color", color.map(|c| format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)));
    set("icon", (!icon.is_empty()).then(|| icon.to_string()));
    set("group", (!group.is_empty()).then(|| group.to_string()));
    if !entry.is_empty() {
        hosts.insert(alias, toml_edit::Item::Table(entry));
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, doc.to_string())
}

pub fn delete_extras(alias: &str) -> std::io::Result<()> {
    let Some(path) = config_dir().map(|dir| dir.join(HOSTS_FILE)) else { return Ok(()) };
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(()) };
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else { return Ok(()) };
    if let Some(hosts) = doc.get_mut("hosts").and_then(|h| h.as_table_mut()) {
        hosts.remove(alias);
    }
    std::fs::write(&path, doc.to_string())
}

/// What's wrong with a form, as a message for the editor. `existing` are
/// the aliases already defined anywhere.
pub fn validate(form: &HostForm, previous_alias: Option<&str>, existing: &[String]) -> Result<(), String> {
    if form.alias.is_empty() {
        return Err("Give the connection a name.".into());
    }
    if form.alias.contains(|c: char| c.is_whitespace() || "*?!#,=\"".contains(c)) {
        return Err("The name can't contain spaces or any of * ? ! # , = \"".into());
    }
    if previous_alias != Some(form.alias.as_str()) && existing.iter().any(|a| a == &form.alias) {
        return Err(format!("A host named \"{}\" already exists.", form.alias));
    }
    if form.hostname.is_empty() {
        return Err("Enter the server's address.".into());
    }
    if form.hostname.contains(char::is_whitespace) || form.user.contains(char::is_whitespace) {
        return Err("The address and user can't contain spaces.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(alias: &str) -> HostForm {
        HostForm {
            alias: alias.into(),
            hostname: "10.0.0.1".into(),
            user: "ion".into(),
            port: Some(2222),
            identity_file: "~/.ssh/my key".into(),
            proxy_jump: String::new(),
        }
    }

    #[test]
    fn upserts_and_keeps_hand_written_lines() {
        let mut file = parse_managed("# mine\n\nHost box\n    HostName old\n    ForwardAgent yes\n");
        upsert(&mut file, &form("box2"), Some("box"));
        let text = render_managed(&file);
        assert_eq!(
            text,
            "# mine\n\nHost box2\n    HostName 10.0.0.1\n    User ion\n    Port 2222\n    IdentityFile \"~/.ssh/my key\"\n    ForwardAgent yes\n"
        );
        upsert(&mut file, &form("other"), None);
        assert_eq!(file.blocks.len(), 2);
        // Reading back and saving again changes nothing.
        let once = render_managed(&file);
        assert_eq!(render_managed(&parse_managed(&once)), once);
    }

    #[test]
    fn detects_existing_includes() {
        assert!(include_covers("Include config.d/*\nHost a\n"));
        assert!(include_covers("Include ~/.ssh/config.d/ionterm\n"));
        assert!(!include_covers("Host a\n  Include config.d/*\n"));
        assert!(!include_covers("Include other\n"));
    }

    #[test]
    fn validates_forms() {
        let existing = vec!["taken".to_string()];
        assert!(validate(&form("new"), None, &existing).is_ok());
        assert!(validate(&form("taken"), None, &existing).is_err());
        assert!(validate(&form("taken"), Some("taken"), &existing).is_ok());
        assert!(validate(&form("has space"), None, &existing).is_err());
        assert!(validate(&HostForm { hostname: String::new(), ..form("x") }, None, &existing).is_err());
    }
}
