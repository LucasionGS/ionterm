//! Port forwards from SSH tabs (`crate::forward`): the picker of the
//! server's ports, starting and stopping forwards, setting a host's saved
//! ones up on connecting, and the port chips in the tab bar (forwards, and
//! ports local programs listen on).

use std::time::{Duration, Instant};

use super::{App, TermWindow, UserEvent};
use crate::dialog::{self, Answer, Dialog};
use crate::forward::{self, Event, Listening, Spec};
use crate::layout::PaneId;
use crate::palette::{Palette, PaletteAction, PaletteItem};
use crate::ssh::{self, Connection};
use crate::tabbar::Chip;

/// How often the ports local programs listen on are looked at again.
const LISTENING_REFRESH: Duration = Duration::from_secs(2);
/// Chip icons: a forward, and a port something listens on here.
const FORWARD_ICON: char = '\u{F04E1}';
const LISTENING_ICON: char = '\u{F059F}';

/// What a port chip stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChipTarget {
    Forward { pane: PaneId, reverse: bool, listen: u16 },
    /// A port a program in a local pane listens on.
    Listening(u16),
}

impl TermWindow {
    /// The active tab's forwards, then the ports its local programs listen
    /// on, as chips.
    pub(super) fn port_chips(&self) -> Vec<(Chip, ChipTarget)> {
        let tab = &self.tabs[self.active_tab];
        let mut chips = Vec::new();
        for pane in &tab.panes {
            for active in &pane.forwards.list {
                let spec = &active.spec;
                chips.push((
                    Chip {
                        icon: FORWARD_ICON,
                        label: spec.label(),
                    },
                    ChipTarget::Forward {
                        pane: pane.id,
                        reverse: spec.reverse,
                        listen: spec.listen,
                    },
                ));
            }
        }
        let mut seen = Vec::new();
        for port in tab.panes.iter().flat_map(|p| &p.listening) {
            if !seen.contains(port) {
                seen.push(*port);
                chips.push((
                    Chip {
                        icon: LISTENING_ICON,
                        label: format!(":{port}"),
                    },
                    ChipTarget::Listening(*port),
                ));
            }
        }
        chips
    }

    /// Re-lays the window out when chips appear or all go, since they can
    /// show the tab bar by themselves.
    pub(super) fn refresh_chips(&mut self, config: &crate::config::Config) {
        let shown = !self.port_chips().is_empty();
        if shown != self.chips_shown {
            self.chips_shown = shown;
            self.relayout(config, false);
        }
        self.window.request_redraw();
    }

    /// Looks at which ports the active tab's local programs listen on,
    /// every `LISTENING_REFRESH`. Returns when to look again.
    pub(super) fn refresh_listening(&mut self, config: &crate::config::Config, now: Instant) -> Option<Instant> {
        if !cfg!(target_os = "linux") {
            return None;
        }
        if self.listening_checked.is_some_and(|at| now.duration_since(at) < LISTENING_REFRESH) {
            return self.listening_checked.map(|at| at + LISTENING_REFRESH);
        }
        self.listening_checked = Some(now);
        let tab = self.active_tab;
        // Only panes running here: a typed ssh's ports are on the server.
        let roots: Vec<(PaneId, u32)> = self.tabs[tab]
            .panes
            .iter()
            .filter(|p| p.connection.is_none() && p.detected_ssh.is_none())
            .filter_map(|p| Some((p.id, p.child_pid()?)))
            .collect();
        let found = forward::local_listening(&roots.iter().map(|r| r.1).collect::<Vec<_>>());
        let mut changed = false;
        for pane in &mut self.tabs[tab].panes {
            let ports = roots
                .iter()
                .find(|r| r.0 == pane.id)
                .and_then(|r| found.get(&r.1))
                .cloned()
                .unwrap_or_default();
            if ports != pane.listening {
                pane.listening = ports;
                changed = true;
            }
        }
        if changed {
            self.refresh_chips(config);
        }
        Some(now + LISTENING_REFRESH)
    }
}

impl App {
    /// The managed SSH connection of the active pane, or a toast saying
    /// forwards need one.
    fn forward_connection(&mut self) -> Option<(PaneId, Connection)> {
        let w = self.window.as_mut()?;
        let pane = w.active_pane();
        match pane.connection.clone().filter(|c| c.managed) {
            Some(connection) => Some((pane.id, connection)),
            None => {
                w.show_toast("Port forwards work in SSH tabs ionterm opened (ctrl+shift+s)");
                None
            }
        }
    }

    /// "Forward port…": looks up the server's listening ports, then shows
    /// them to pick from.
    pub(super) fn open_forward_picker(&mut self) {
        let Some((pane, connection)) = self.forward_connection() else { return };
        if let Some(w) = &mut self.window {
            w.show_toast(format!("Looking at the ports {} listens on…", connection.alias));
        }
        let remote = crate::files::remote::Remote::new(&connection);
        self.forward_work(pane, move || Event::Listed(forward::remote_listening(&remote)));
    }

    fn forward_work(&self, pane: PaneId, job: impl FnOnce() -> Event + Send + 'static) {
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new().name(format!("forward-{pane}")).spawn(move || {
            let _ = proxy.send_event(UserEvent::Forward(pane, job()));
        });
        if let Err(error) = spawned {
            tracing::warn!("could not start a forward: {error}");
        }
    }

    pub(super) fn forward_event(&mut self, pane: PaneId, event: Event) {
        let Some(w) = &mut self.window else { return };
        let Some(p) = w.tabs.iter_mut().find_map(|t| t.pane_mut(pane)) else { return };
        let host = p.connection.as_ref().map(|c| c.alias.clone()).unwrap_or_default();
        match event {
            Event::Listed(Ok(ports)) => {
                let items = picker_items(&host, &ports, p);
                w.toast = None;
                let placeholder = "Forward a port of the server…";
                w.palette = Some(Palette::chooser(items, placeholder, "↵ forward   esc close"));
                w.menu = None;
                w.window.request_redraw();
            }
            Event::Listed(Err(message)) => w.show_toast(format!("Couldn't list the ports on {host}: {message}")),
            Event::Started(asked, Ok(active)) => {
                let moved = !asked.reverse && active.spec.listen != asked.listen;
                let mut message = format!("Forwarding {}", active.spec.describe(&host));
                if moved && forward::is_privileged(asked.listen) {
                    message.push_str(&format!(" (ports below 1024 need root here, so it's {})", active.spec.listen));
                } else if moved {
                    message.push_str(&format!(" ({} was taken here)", asked.listen));
                }
                p.forwards.push(active);
                w.show_toast(message);
                w.refresh_chips(&self.config);
            }
            Event::Started(asked, Err(message)) => w.show_toast(format!("Couldn't forward {}: {message}", asked.label())),
            Event::NoConnection(count) => {
                let what = if count == 1 { "forward".to_string() } else { format!("{count} forwards") };
                w.show_toast(format!("{host}'s saved {what} didn't start: it never connected"));
            }
        }
    }

    /// Starts a forward over a pane's connection.
    pub(super) fn start_forward(&mut self, pane: PaneId, spec: Spec) {
        let connection = self
            .window
            .as_ref()
            .and_then(|w| w.tabs.iter().find_map(|t| t.pane(pane)))
            .and_then(|p| p.connection.clone());
        let Some(connection) = connection else { return };
        self.forward_work(pane, move || {
            let asked = spec.clone();
            Event::Started(asked, forward::start(&connection, spec))
        });
    }

    /// Stops a pane's forward listening on `listen`.
    pub(super) fn stop_forward(&mut self, pane: PaneId, reverse: bool, listen: u16) {
        let Some(w) = &mut self.window else { return };
        let Some(p) = w.tabs.iter_mut().find_map(|t| t.pane_mut(pane)) else { return };
        if p.forwards.remove(reverse, listen) {
            let place = if reverse { "the server's" } else { "local" };
            w.show_toast(format!("Stopped forwarding {place} port {listen}"));
            w.refresh_chips(&self.config);
        }
    }

    /// Sets up forwards once a new connection has logged in: the host's
    /// saved ones, or those a reconnecting pane had (`cancel_first`, since
    /// the shared connection may still hold them).
    pub(super) fn start_forwards_when_connected(&self, pane: PaneId, connection: Connection, specs: Vec<Spec>, cancel_first: bool) {
        if specs.is_empty() {
            return;
        }
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new().name(format!("forwards-{pane}")).spawn(move || {
            if !forward::wait_for_master(&connection) {
                let _ = proxy.send_event(UserEvent::Forward(pane, Event::NoConnection(specs.len())));
                return;
            }
            if cancel_first {
                forward::cancel_quietly(&connection, &specs);
            }
            for spec in specs {
                let event = Event::Started(spec.clone(), forward::start(&connection, spec));
                let _ = proxy.send_event(UserEvent::Forward(pane, event));
            }
        });
        if let Err(error) = spawned {
            tracing::warn!("could not set up forwards: {error}");
        }
    }

    /// A new pane connected to a host: its saved forwards.
    pub(super) fn start_saved_forwards(&self, pane: PaneId, connection: &Connection) {
        if !connection.managed {
            return;
        }
        let extras = ssh::load_extras();
        let Some(saved) = extras.get(&connection.alias) else { return };
        let specs: Vec<Spec> = saved
            .forwards
            .iter()
            .filter_map(|text| Spec::parse(text).map_err(|e| tracing::warn!("ignoring forward {text:?}: {e}")).ok())
            .collect();
        self.start_forwards_when_connected(pane, connection.clone(), specs, false);
    }

    /// "Another port…" and "From the server…": the forward typed in.
    pub(super) fn ask_forward(&mut self, reverse: bool) {
        let Some((pane, connection)) = self.forward_connection() else { return };
        let Some(w) = &mut self.window else { return };
        w.palette = None;
        let (title, message) = if reverse {
            (
                format!("Forward from {}", connection.alias),
                "The server's port, then where it goes here: 9000 (to this computer's 9000), 9000:3000, or 9000:host:port.",
            )
        } else {
            (
                format!("Forward to {}", connection.alias),
                "The port on the server: 3000, local:remote (8080:80), or local:host:port to reach a host from there.",
            )
        };
        w.dialog = Some(
            Dialog::new(
                title,
                message,
                vec![dialog::Button::new("Forward", 'f'), dialog::Button::new("Cancel", 'c')],
                super::files::Purpose::Forward { pane, reverse },
            )
            .with_input(""),
        );
        w.window.request_redraw();
    }

    pub(super) fn forward_answer(&mut self, pane: PaneId, reverse: bool, answer: Answer) {
        if answer.button != Some(0) {
            return;
        }
        let text = if reverse { format!("R {}", answer.text.trim()) } else { answer.text.trim().to_string() };
        match Spec::parse(&text) {
            Ok(spec) => self.start_forward(pane, spec),
            Err(message) => {
                self.ask_forward(reverse);
                if let Some(w) = &mut self.window
                    && let Some(open) = &mut w.dialog
                {
                    open.error = Some(message);
                    if let Some(input) = &mut open.input {
                        input.set(answer.text);
                    }
                }
            }
        }
    }

    /// A chip was clicked: open the port in the browser, or (`stop`,
    /// right or middle click) stop the forward.
    pub(super) fn chip_click(&mut self, index: usize, stop: bool) {
        let Some(w) = &mut self.window else { return };
        let Some((_, target)) = w.port_chips().into_iter().nth(index) else { return };
        match (target, stop) {
            (ChipTarget::Forward { pane, reverse, listen }, true) => self.stop_forward(pane, reverse, listen),
            (ChipTarget::Forward { reverse: true, listen, .. }, false) => {
                w.show_toast(format!("The server listens on {listen}; right-click stops it"));
            }
            (ChipTarget::Forward { listen: port, .. } | ChipTarget::Listening(port), false) => {
                crate::links::open(&format!("http://localhost:{port}"));
            }
            (ChipTarget::Listening(port), true) => w.show_toast(format!("A program in this tab listens on {port}")),
        }
    }

    /// What a chip does, shown when the mouse is over it.
    pub(super) fn chip_hint(&mut self, index: usize) {
        let Some(w) = &mut self.window else { return };
        let Some((_, target)) = w.port_chips().into_iter().nth(index) else { return };
        let message = match target {
            ChipTarget::Forward { pane, reverse, listen } => {
                let Some(p) = w.tabs.iter().find_map(|t| t.pane(pane)) else { return };
                let host = p.connection.as_ref().map(|c| c.alias.as_str()).unwrap_or_default();
                let Some(active) = p.forwards.list.iter().find(|a| a.spec.reverse == reverse && a.spec.listen == listen) else { return };
                let open = if reverse { "" } else { "click opens it, " };
                format!("{}: {open}right-click stops it", active.spec.describe(host))
            }
            ChipTarget::Listening(port) => format!("Something here listens on {port}: click opens http://localhost:{port}"),
        };
        w.show_toast(message);
    }
}

/// The picker: the server's listening ports not forwarded yet, typing one
/// in either way, and the forwards to stop.
fn picker_items(host: &str, ports: &[Listening], pane: &crate::pane::Pane) -> Vec<PaletteItem> {
    // Each is forwarded to the same port here (or the next free one).
    let forwarded = |port: u16| pane.forwards.list.iter().any(|a| !a.spec.reverse && a.spec.host == "localhost" && a.spec.port == port);
    let mut items: Vec<PaletteItem> = ports
        .iter()
        .filter(|l| !forwarded(l.port) && l.port != 22)
        .map(|l| {
            let mut detail = l.addresses.join(", ");
            if let Some(process) = &l.process {
                detail = format!("{process} · {detail}");
            }
            PaletteItem {
                title: format!(":{}", l.port),
                detail,
                group: "Listening".into(),
                icon: FORWARD_ICON,
                color: None,
                action: PaletteAction::Forward(l.port.to_string()),
            }
        })
        .collect();
    for active in &pane.forwards.list {
        items.push(PaletteItem {
            title: format!("Stop {}", active.spec.label()),
            detail: active.spec.describe(host),
            group: "Forwarding".into(),
            icon: '\u{F04DB}',
            color: None,
            action: PaletteAction::Unforward(active.spec.reverse, active.spec.listen),
        });
    }
    items.extend([
        PaletteItem {
            title: "Another port…".into(),
            detail: "local:host:port".into(),
            group: String::new(),
            icon: '\u{F0415}',
            color: None,
            action: PaletteAction::ForwardOther(false),
        },
        PaletteItem {
            title: format!("From {host} to this computer…"),
            detail: "the server listens".into(),
            group: String::new(),
            icon: '\u{F04E1}',
            color: None,
            action: PaletteAction::ForwardOther(true),
        },
    ]);
    items
}
