//! Port forwards over SSH connections ionterm opened, and the ports local
//! programs listen on.
//!
//! Forwards are added to and removed from the host's shared connection
//! (`ssh -O forward` / `ssh -O cancel`), so they need no login of their own.
//! Without connection sharing (Windows) each forward is a separate `ssh -N`.
//! The pane that owns them cancels them when it closes: the shared
//! connection outlives the tab for a while, and would keep them otherwise.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::files::remote::Remote;
use crate::ssh::Connection;

/// How long a new connection may take to log in before its saved forwards
/// give up (typing a password included).
const MASTER_WAIT: Duration = Duration::from_secs(120);

/// One forward, as `hosts.toml` and the picker write it: `3000`,
/// `8080:80`, `8080:db.internal:5432`, or with `R ` in front for the other
/// way (the server listens and connects back here).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// The server listens, and connections come back to this computer.
    pub reverse: bool,
    /// The port listened on: here, or on the server when `reverse`.
    pub listen: u16,
    /// Where connections go, as seen from the other end.
    pub host: String,
    pub port: u16,
}

impl Spec {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let (reverse, rest) = match text.strip_prefix(['R', 'r']).filter(|r| r.starts_with([' ', ':'])) {
            Some(rest) => (true, rest.trim_start_matches([' ', ':'])),
            None => (false, text),
        };
        let port = |word: &str| {
            word.trim()
                .parse::<u16>()
                .ok()
                .filter(|&p| p > 0)
                .ok_or_else(|| format!("{word:?} isn't a port number."))
        };
        let parts: Vec<&str> = rest.split(':').collect();
        let (listen, host, target) = match parts.as_slice() {
            [p] => (port(p)?, "localhost", port(p)?),
            [l, p] => (port(l)?, "localhost", port(p)?),
            [l, h, p] if !h.trim().is_empty() => (port(l)?, h.trim(), port(p)?),
            _ => return Err(format!("Can't read {text:?}: write a port (3000), local:remote (8080:80) or local:host:port.")),
        };
        Ok(Self {
            reverse,
            listen,
            host: host.to_string(),
            port: target,
        })
    }

    /// As `parse` reads it back.
    pub fn text(&self) -> String {
        let prefix = if self.reverse { "R " } else { "" };
        match (self.host.as_str(), self.listen == self.port) {
            ("localhost", true) => format!("{prefix}{}", self.listen),
            ("localhost", false) => format!("{prefix}{}:{}", self.listen, self.port),
            (host, _) => format!("{prefix}{}:{host}:{}", self.listen, self.port),
        }
    }

    /// `-L 3000:localhost:3000` or `-R …`.
    fn ssh_args(&self) -> [String; 2] {
        let flag = if self.reverse { "-R" } else { "-L" };
        [flag.into(), format!("{}:{}:{}", self.listen, self.host, self.port)]
    }

    /// Short, for the tab bar: `:3000`, `:8080 → 80`, `:8080 → db:5432`,
    /// and `server :9000 → 3000` the other way.
    pub fn label(&self) -> String {
        let target = if self.host == "localhost" { self.port.to_string() } else { format!("{}:{}", self.host, self.port) };
        let start = if self.reverse { format!("server :{}", self.listen) } else { format!(":{}", self.listen) };
        if !self.reverse && self.host == "localhost" && self.listen == self.port {
            start
        } else {
            format!("{start} → {target}")
        }
    }

    /// In a sentence: `localhost:3000 → ionserver:3000`.
    pub fn describe(&self, server: &str) -> String {
        let to = if self.host == "localhost" { String::new() } else { format!(" ({}:{} from there)", self.host, self.port) };
        if self.reverse {
            format!("{server}:{} → this computer's {}:{}", self.listen, self.host, self.port)
        } else if to.is_empty() {
            format!("localhost:{} → {server}:{}", self.listen, self.port)
        } else {
            format!("localhost:{} → {server}{to}", self.listen)
        }
    }
}

/// Background work on forwards, reported to the event loop.
#[derive(Debug)]
pub enum Event {
    /// The server's listening ports, for the picker.
    Listed(Result<Vec<Listening>, String>),
    /// A forward started (maybe on another local port), or why not.
    Started(Spec, Result<Active, String>),
    /// The connection never came up, so its saved forwards didn't start.
    NoConnection(usize),
}

/// A forward that's running.
#[derive(Debug)]
pub struct Active {
    pub spec: Spec,
    /// The separate `ssh -N` carrying it, without connection sharing.
    tunnel: Option<Child>,
}

/// A pane's forwards; dropping it cancels them.
#[derive(Debug, Default)]
pub struct Forwards {
    pub list: Vec<Active>,
    connection: Option<Connection>,
}

impl Forwards {
    pub fn new(connection: Option<Connection>) -> Self {
        Self {
            list: Vec::new(),
            connection,
        }
    }

    pub fn push(&mut self, active: Active) {
        self.list.push(active);
    }

    /// Stops the forward listening on `listen` (on the server when
    /// `reverse`), without waiting for it. Returns whether there was one.
    pub fn remove(&mut self, reverse: bool, listen: u16) -> bool {
        let Some(index) = self.list.iter().position(|a| a.spec.reverse == reverse && a.spec.listen == listen) else {
            return false;
        };
        let active = self.list.remove(index);
        if let Some(connection) = &self.connection {
            stop(connection, active);
        }
        true
    }

    /// Forgets the forwards without cancelling them: the connection they
    /// were on ended, and a reconnect cancels and sets them up again.
    pub fn forget(&mut self) -> Vec<Spec> {
        let specs = self.list.iter().map(|a| a.spec.clone()).collect();
        for active in std::mem::take(&mut self.list) {
            if let Some(mut tunnel) = active.tunnel {
                let _ = tunnel.kill();
                std::thread::spawn(move || tunnel.wait());
            }
        }
        specs
    }

    /// Takes the specs out, cancelling them.
    pub fn take_specs(&mut self) -> Vec<Spec> {
        let specs = self.list.iter().map(|a| a.spec.clone()).collect();
        for active in std::mem::take(&mut self.list) {
            if let Some(connection) = &self.connection {
                stop(connection, active);
            }
        }
        specs
    }
}

impl Drop for Forwards {
    fn drop(&mut self) {
        self.take_specs();
    }
}

/// Waits until the host's shared connection is up (it logs in with the
/// tab's session). False when it never came, or there's no sharing.
pub fn wait_for_master(connection: &Connection) -> bool {
    let Some(argv) = connection.control_argv("check", &[]) else { return true };
    let until = Instant::now() + MASTER_WAIT;
    while Instant::now() < until {
        let ok = quiet(&argv).status().is_ok_and(|s| s.success());
        if ok {
            return true;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    false
}

/// Adds a forward. A local port that's taken (or below 1024, which needs
/// root) moves to a free one, so the returned spec may listen elsewhere
/// than asked.
pub fn start(connection: &Connection, mut spec: Spec) -> Result<Active, String> {
    if !spec.reverse {
        spec.listen = free_local_port(spec.listen).ok_or("No local port is free.")?;
    }
    let args = spec.ssh_args();
    match connection.control_argv("forward", &args) {
        Some(argv) => {
            let output = quiet(&argv).output().map_err(|e| format!("Could not run ssh: {e}"))?;
            if !output.status.success() {
                return Err(ssh_error(&output.stderr));
            }
            Ok(Active { spec, tunnel: None })
        }
        None => {
            let argv = connection.tunnel_argv(&args);
            let mut child = quiet(&argv).spawn().map_err(|e| format!("Could not run ssh: {e}"))?;
            // A tunnel that fails (port taken, login refused) exits at once.
            std::thread::sleep(Duration::from_millis(1500));
            if let Ok(Some(_)) = child.try_wait() {
                let mut stderr = Vec::new();
                if let Some(mut pipe) = child.stderr.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_end(&mut stderr);
                }
                return Err(ssh_error(&stderr));
            }
            Ok(Active { spec, tunnel: Some(child) })
        }
    }
}

/// Cancels a forward in the background.
fn stop(connection: &Connection, active: Active) {
    if let Some(mut tunnel) = active.tunnel {
        let _ = tunnel.kill();
        std::thread::spawn(move || tunnel.wait());
        return;
    }
    let Some(argv) = connection.control_argv("cancel", &active.spec.ssh_args()) else { return };
    if let Ok(mut child) = quiet(&argv).spawn() {
        std::thread::spawn(move || child.wait());
    }
}

/// Cancels forwards left in the shared connection by a session that ended,
/// before setting them up again; errors (nothing to cancel) don't matter.
pub fn cancel_quietly(connection: &Connection, specs: &[Spec]) {
    for spec in specs {
        if let Some(argv) = connection.control_argv("cancel", &spec.ssh_args()) {
            let _ = quiet(&argv).status();
        }
    }
}

fn quiet(argv: &[String]) -> Command {
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
}

/// ssh's complaint, shortened: "Port forwarding failed." rather than the
/// `mux_client_forward:` noise.
fn ssh_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("ssh refused the forward.");
    let line = line.rsplit(": ").next().unwrap_or(line);
    let mut message = line.to_string();
    if let Some(first) = message.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    if !message.ends_with('.') {
        message.push('.');
    }
    message
}

/// A local port near `wanted` that this user can listen on: `wanted` or
/// the next free one. Ports below 1024 need root, so without it they move
/// up by 8000 (443 → 8443, 80 → 8080). Failing all that, any free port.
pub fn free_local_port(wanted: u16) -> Option<u16> {
    let can_bind = |port: u16| std::net::TcpListener::bind(("127.0.0.1", port)).is_ok();
    let near = |start: u16| (start..=start.saturating_add(50)).find(|&port| can_bind(port));
    if can_bind(wanted) {
        return Some(wanted);
    }
    let start = if wanted < 1024 { wanted + 8000 } else { wanted };
    near(start).or_else(|| {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).ok()?;
        Some(listener.local_addr().ok()?.port())
    })
}

/// Whether this user may listen on a port (below 1024 needs root, unless
/// the system lowered the limit).
pub fn is_privileged(port: u16) -> bool {
    #[cfg(target_os = "linux")]
    {
        let start = std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(1024);
        u32::from(port) < start && rustix::process::getuid().as_raw() != 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        cfg!(unix) && port < 1024
    }
}

/// A port something listens on, on the server or here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listening {
    pub port: u16,
    /// The addresses it listens on (`127.0.0.1`, `*`…).
    pub addresses: Vec<String>,
    /// The program, when it's yours to see.
    pub process: Option<String>,
}

/// The TCP ports the server listens on: `ss` when it's there, else
/// `/proc/net/tcp`.
pub fn remote_listening(remote: &Remote) -> Result<Vec<Listening>, String> {
    let output = remote.run("if command -v ss >/dev/null 2>&1; then ss -ltnp 2>/dev/null; else cat /proc/net/tcp /proc/net/tcp6 2>/dev/null; fi\n")?;
    Ok(parse_listening(&String::from_utf8_lossy(&output)))
}

/// Reads `ss -ltnp` or `/proc/net/tcp` output.
fn parse_listening(text: &str) -> Vec<Listening> {
    let mut found: Vec<Listening> = Vec::new();
    let mut add = |port: u16, address: String, process: Option<String>| match found.iter_mut().find(|l| l.port == port) {
        Some(known) => {
            if !known.addresses.contains(&address) {
                known.addresses.push(address);
            }
            known.process = known.process.take().or(process);
        }
        None => found.push(Listening {
            port,
            addresses: vec![address],
            process,
        }),
    };
    for line in text.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        // ss: LISTEN 0 4096 127.0.0.1:3000 0.0.0.0:* users:(("node",pid=1,fd=20))
        if words.first() == Some(&"LISTEN") && words.len() >= 4 {
            let Some((address, port)) = words[3].rsplit_once(':') else { continue };
            let Ok(port) = port.parse() else { continue };
            let process = words.iter().find_map(|w| w.strip_prefix("users:((\"")).and_then(|w| w.split('"').next()).map(String::from);
            add(port, address.trim_matches(['[', ']']).to_string(), process);
            continue;
        }
        // /proc/net/tcp: sl local_address rem_address st … (0A = listening)
        if words.len() > 3 && words[3] == "0A" {
            let Some((address, port)) = words[1].split_once(':') else { continue };
            let Ok(port) = u16::from_str_radix(port, 16) else { continue };
            add(port, proc_address(address), None);
        }
    }
    found.sort_by_key(|l| l.port);
    found
}

/// A `/proc/net/tcp` address in hex (IPv4 little-endian), readable.
fn proc_address(hex: &str) -> String {
    if hex.chars().all(|c| c == '0') {
        return "*".into();
    }
    if hex.len() == 8
        && let Ok(n) = u32::from_str_radix(hex, 16)
    {
        return std::net::Ipv4Addr::from(n.swap_bytes()).to_string();
    }
    "::".into()
}

/// The TCP ports that processes under each of `roots` (a pane's shell)
/// listen on. Linux only.
pub fn local_listening(roots: &[u32]) -> HashMap<u32, Vec<u16>> {
    #[cfg(target_os = "linux")]
    {
        linux::listening(roots)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = roots;
        HashMap::new()
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::{HashMap, HashSet};

    /// Listening sockets by inode, then each pane's processes' open
    /// sockets: what's in both listens for that pane.
    pub fn listening(roots: &[u32]) -> HashMap<u32, Vec<u16>> {
        let mut out = HashMap::new();
        if roots.is_empty() {
            return out;
        }
        let sockets = listening_inodes();
        if sockets.is_empty() {
            return out;
        }
        let parents = parent_map();
        for &root in roots {
            let mut ports: Vec<u16> = descendants(root, &parents)
                .into_iter()
                .flat_map(socket_inodes)
                .filter_map(|inode| sockets.get(&inode).copied())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            ports.sort_unstable();
            if !ports.is_empty() {
                out.insert(root, ports);
            }
        }
        out
    }

    fn listening_inodes() -> HashMap<u64, u16> {
        let mut map = HashMap::new();
        for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let Ok(text) = std::fs::read_to_string(file) else { continue };
            for line in text.lines().skip(1) {
                let words: Vec<&str> = line.split_whitespace().collect();
                if words.len() > 9 && words[3] == "0A"
                    && let Some(port) = words[1].rsplit_once(':').and_then(|(_, p)| u16::from_str_radix(p, 16).ok())
                    && let Ok(inode) = words[9].parse()
                {
                    map.insert(inode, port);
                }
            }
        }
        map
    }

    /// Every process's parent.
    fn parent_map() -> HashMap<u32, Vec<u32>> {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let Ok(entries) = std::fs::read_dir("/proc") else { return children };
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
            // The name may hold spaces and parentheses: the fields after it start past the last ')'.
            let Some(after) = stat.rsplit_once(')').map(|(_, rest)| rest) else { continue };
            if let Some(parent) = after.split_whitespace().nth(1).and_then(|p| p.parse().ok()) {
                children.entry(parent).or_default().push(pid);
            }
        }
        children
    }

    fn descendants(root: u32, children: &HashMap<u32, Vec<u32>>) -> Vec<u32> {
        let mut all = vec![root];
        let mut index = 0;
        while index < all.len() {
            if let Some(kids) = children.get(&all[index]) {
                all.extend(kids);
            }
            index += 1;
        }
        all
    }

    fn socket_inodes(pid: u32) -> Vec<u64> {
        let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { return Vec::new() };
        entries
            .flatten()
            .filter_map(|e| std::fs::read_link(e.path()).ok())
            .filter_map(|target| {
                let text = target.to_string_lossy().into_owned();
                text.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_parse_and_print() {
        let spec = Spec::parse("3000").unwrap();
        assert_eq!((spec.reverse, spec.listen, spec.host.as_str(), spec.port), (false, 3000, "localhost", 3000));
        assert_eq!(spec.label(), ":3000");
        let spec = Spec::parse("8080:db.internal:5432").unwrap();
        assert_eq!(spec.text(), "8080:db.internal:5432");
        assert_eq!(spec.label(), ":8080 → db.internal:5432");
        let spec = Spec::parse("R 9000:3000").unwrap();
        assert!(spec.reverse);
        assert_eq!(spec.text(), "R 9000:3000");
        assert_eq!(spec.label(), "server :9000 → 3000");
        assert_eq!(Spec::parse("r:22").unwrap().text(), "R 22");
        for bad in ["", "x", "0", "70000", "1:2:3:4", "1::2"] {
            assert!(Spec::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn reads_listening_ports() {
        let ss = "State  Recv-Q Send-Q Local Address:Port Peer Address:Port Process\n\
                  LISTEN 0      4096       127.0.0.1:3000      0.0.0.0:*    users:((\"node\",pid=12,fd=20))\n\
                  LISTEN 0      128          0.0.0.0:22        0.0.0.0:*\n\
                  LISTEN 0      128             [::]:22           [::]:*\n";
        let found = parse_listening(ss);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0], Listening { port: 22, addresses: vec!["0.0.0.0".into(), "::".into()], process: None });
        assert_eq!(found[1].process.as_deref(), Some("node"));

        let proc = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
                    0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345\n\
                    1: 0100007F:0BB9 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 12346\n";
        assert_eq!(parse_listening(proc), vec![Listening { port: 3000, addresses: vec!["127.0.0.1".into()], process: None }]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn privileged_ports_move_up() {
        if rustix::process::getuid().as_raw() == 0 || !is_privileged(443) {
            return;
        }
        let port = free_local_port(443).unwrap();
        assert!(port >= 1024, "{port}");
        // Taken ports move to the next free one.
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = taken.local_addr().unwrap().port();
        assert_ne!(free_local_port(busy), Some(busy));
    }

    #[test]
    fn shortens_ssh_errors() {
        assert_eq!(ssh_error(b"mux_client_forward: forwarding request failed: Port forwarding failed\n"), "Port forwarding failed.");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_ports_a_process_listens_on() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id();
        let found = local_listening(&[me]);
        assert!(found.get(&me).is_some_and(|ports| ports.contains(&port)), "{found:?}");
    }
}
