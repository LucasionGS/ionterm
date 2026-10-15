//! Reading host entries from OpenSSH's config, following `Include`.
//!
//! Only what ionterm shows or needs is extracted; `ssh` itself still reads
//! the full config when connecting, so nothing here has to be complete.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How deep `Include` may nest before ionterm stops following it.
const MAX_INCLUDE_DEPTH: usize = 8;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshHost {
    /// The `Host` alias, as typed after `ssh`.
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    pub proxy_jump: Option<String>,
    /// Set when the host runs its own remote command; ionterm then can't
    /// add its bootstrap.
    pub remote_command: bool,
    /// The file the host was defined in.
    pub source: PathBuf,
}

impl SshHost {
    /// `user@host:port`, leaving out what isn't set.
    pub fn target(&self) -> String {
        let host = self.hostname.as_deref().unwrap_or(&self.alias);
        let mut s = String::new();
        if let Some(user) = &self.user {
            s.push_str(user);
            s.push('@');
        }
        s.push_str(host);
        if let Some(port) = self.port.filter(|&p| p != 22) {
            s.push_str(&format!(":{port}"));
        }
        s
    }
}

pub fn ssh_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".ssh"))
}

/// Every concrete host alias in `~/.ssh/config`, in file order. Wildcard
/// patterns (`Host *`) are settings, not hosts, and are skipped.
pub fn load_hosts() -> Vec<SshHost> {
    let Some(dir) = ssh_dir() else { return Vec::new() };
    let mut hosts: Vec<SshHost> = Vec::new();
    let mut index = HashMap::new();
    parse_file(&dir.join("config"), &dir, 0, &mut hosts, &mut index);
    hosts
}

fn parse_file(path: &Path, ssh_dir: &Path, depth: usize, hosts: &mut Vec<SshHost>, index: &mut HashMap<String, usize>) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    // Hosts the current block applies to (indexes into `hosts`).
    let mut current: Vec<usize> = Vec::new();
    for line in text.lines() {
        let Some((keyword, args)) = split_line(line) else { continue };
        match keyword.as_str() {
            "host" => {
                current = args
                    .iter()
                    .filter(|pattern| !pattern.contains(['*', '?', '!']))
                    .map(|alias| {
                        *index.entry(alias.clone()).or_insert_with(|| {
                            hosts.push(SshHost {
                                alias: alias.clone(),
                                source: path.to_path_buf(),
                                ..SshHost::default()
                            });
                            hosts.len() - 1
                        })
                    })
                    .collect();
            }
            // Conditional blocks can't be evaluated here; their settings
            // belong to no particular alias.
            "match" => current.clear(),
            "include" if depth < MAX_INCLUDE_DEPTH => {
                for pattern in &args {
                    for file in expand_include(pattern, ssh_dir) {
                        parse_file(&file, ssh_dir, depth + 1, hosts, index);
                    }
                }
            }
            _ => {
                let value = args.join(" ");
                for &i in &current {
                    // As in ssh, the first value given for a setting wins.
                    let host = &mut hosts[i];
                    match keyword.as_str() {
                        "hostname" => host.hostname.get_or_insert(value.clone()),
                        "user" => host.user.get_or_insert(value.clone()),
                        "identityfile" => host.identity_file.get_or_insert(value.clone()),
                        "proxyjump" => host.proxy_jump.get_or_insert(value.clone()),
                        "port" => {
                            if host.port.is_none() {
                                host.port = value.parse().ok();
                            }
                            continue;
                        }
                        "remotecommand" => {
                            host.remote_command |= !value.eq_ignore_ascii_case("none");
                            continue;
                        }
                        _ => continue,
                    };
                }
            }
        }
    }
}

/// Splits a config line into a lowercase keyword and its arguments. Values
/// may follow `=` or whitespace, and may be double-quoted.
fn split_line(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let split_at = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let keyword = line[..split_at].to_ascii_lowercase();
    let rest = line[split_at..].trim_start_matches(|c: char| c.is_whitespace() || c == '=');
    let mut args = Vec::new();
    let mut chars = rest.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut arg = String::new();
        if c == '"' {
            chars.next();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                arg.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                arg.push(c);
                chars.next();
            }
        }
        args.push(arg);
    }
    Some((keyword, args))
}

/// Resolves an `Include` argument: `~` expanded, relative to `~/.ssh`,
/// with `*` and `?` wildcards in the file name.
fn expand_include(pattern: &str, ssh_dir: &Path) -> Vec<PathBuf> {
    let expanded = match pattern.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map_or_else(|| PathBuf::from(pattern), |home| home.join(rest)),
        None => PathBuf::from(pattern),
    };
    let path = if expanded.is_absolute() { expanded } else { ssh_dir.join(expanded) };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let Some(dir) = path.parent() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| p.file_name().is_some_and(|n| wildcard(&name, &n.to_string_lossy())))
        .collect();
    files.sort();
    files
}

/// Shell-style `*`/`?` matching.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_hosts_and_includes() {
        let dir = std::env::temp_dir().join(format!("ionterm-ssh-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config.d")).unwrap();
        std::fs::write(
            dir.join("config"),
            "Include config.d/*\n\nHost web web2\n  HostName example.com\n  User = deploy\n  Port 2222\n\nHost *\n  User root\n",
        )
        .unwrap();
        std::fs::write(dir.join("config.d/extra"), "Host db\n    Hostname \"10.0.0.5\"\n    RemoteCommand tmux\n").unwrap();

        let mut hosts = Vec::new();
        parse_file(&dir.join("config"), &dir, 0, &mut hosts, &mut HashMap::new());
        let aliases: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, ["db", "web", "web2"]);
        assert_eq!(hosts[0].hostname.as_deref(), Some("10.0.0.5"));
        assert!(hosts[0].remote_command);
        assert_eq!(hosts[1].target(), "deploy@example.com:2222");
        // `Host *` settings aren't attributed to concrete aliases.
        assert_eq!(hosts[2].user.as_deref(), Some("deploy"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn matches_wildcards() {
        assert!(wildcard("*.conf", "a.conf"));
        assert!(wildcard("*", "anything"));
        assert!(!wildcard("*.conf", "a.txt"));
        assert!(wildcard("h?st", "host"));
    }
}
