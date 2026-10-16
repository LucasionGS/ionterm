//! Remote control commands (`ionterm @ …`), run on the event loop: see
//! `crate::remote` for the socket and the client.

use std::path::PathBuf;

use super::{App, Content, NewWindow};
use crate::input::Action;
use crate::layout::{Direction, PaneId};
use crate::palette::PaneTarget;
use crate::remote::{self, BAD_ARGUMENTS, Parsed, Request};
use crate::ssh;

/// The answer to a command: exit code and output.
type Answer = (u8, String);

fn bad(message: impl Into<String>) -> Answer {
    (BAD_ARGUMENTS, message.into())
}

fn failed(message: impl Into<String>) -> Answer {
    (1, message.into())
}

impl App {
    pub(super) fn remote_request(&mut self, request: Request) {
        // Commands go to the caller's window, or else the one used last.
        let caller = request.pane.is_some_and(|pane| self.select_window_of_pane(pane));
        if !caller && let Some(id) = self.last_focused {
            self.select_window(id);
        }
        // The answer waits until the window is open.
        if request.args.first().is_some_and(|c| c == "new-window") {
            return self.remote_new_window(request);
        }
        let (code, text) = if self.window.is_some() {
            self.remote_command(request.pane, &request.args)
        } else {
            failed("The window is closing.")
        };
        request.reply(code, text);
    }

    fn remote_command(&mut self, caller: Option<PaneId>, args: &[String]) -> Answer {
        let Some((command, args)) = args.split_first() else { return bad(remote::USAGE) };
        // ssh's own options aren't ours to parse.
        if matches!(command.as_str(), "host" | "ssh") {
            return self.remote_connect(command, args);
        }
        let (flags, valued): (&[&str], &[&str]) = match command.as_str() {
            "ls" | "list" => (&[], &[]),
            "version" => {
                let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
                return (0, format!("ionterm {}\n{exe}\n", crate::VERSION));
            }
            "move-tab" => (&["--new-window"], &["-p", "--pane", "--window"]),
            "move-pane" => (&["--new-tab", "--new-window"], &["-p", "--pane", "--tab", "--window"]),
            "send-text" | "get-text" | "focus" | "close" | "set-title" | "action" => (&["-a", "--all"], &["-p", "--pane"]),
            "new-tab" => (&[], &["--cwd", "--title"]),
            "split" => (&["--right", "--down"], &["-p", "--pane", "--cwd"]),
            "help" => return (0, remote::USAGE.into()),
            other => return bad(format!("Unknown command: {other}.\n\n{}", remote::USAGE)),
        };
        let parsed = match Parsed::parse(args, flags, valued) {
            Ok(parsed) => parsed,
            Err(message) => return bad(message),
        };
        // A pane named with -p may be in another window.
        if let Some(id) = parsed.value(&["-p", "--pane"]).and_then(|p| p.parse().ok()) {
            self.select_window_of_pane(id);
        }
        let pane = match self.remote_target(caller, parsed.value(&["-p", "--pane"])) {
            Ok(pane) => pane,
            Err(answer) => return answer,
        };
        match command.as_str() {
            "ls" | "list" => (0, self.remote_list()),
            "send-text" => {
                if parsed.words.is_empty() {
                    return bad("Nothing to send. Usage: ionterm @ send-text [-p PANE] <text...>");
                }
                let text = remote::unescape(&parsed.words.join(" "));
                self.remote_send_text(pane, &text)
            }
            "get-text" => self.remote_get_text(pane, parsed.has(&["-a", "--all"])),
            "new-tab" => {
                let (cwd, title) = (parsed.value(&["--cwd"]).map(PathBuf::from), parsed.value(&["--title"]).map(str::to_string));
                self.remote_new_tab(parsed.words, cwd, title)
            }
            "split" => {
                let direction = match (parsed.has(&["--right"]), parsed.has(&["--down"])) {
                    (true, false) => Some(Direction::Horizontal),
                    (false, true) => Some(Direction::Vertical),
                    (false, false) => None,
                    (true, true) => return bad("Pick one of --right and --down."),
                };
                let cwd = parsed.value(&["--cwd"]).map(PathBuf::from);
                self.remote_split(pane, direction, parsed.words, cwd)
            }
            "focus" => {
                self.focus_pane(pane);
                if let Some(w) = &self.window {
                    w.window.focus_window();
                }
                (0, String::new())
            }
            "close" => {
                self.remove_pane(pane);
                (0, String::new())
            }
            "move-tab" => self.remote_move_tab(pane, &parsed),
            "move-pane" => self.remote_move_pane(pane, &parsed),
            "set-title" => {
                let title = parsed.words.join(" ");
                let Some(w) = &mut self.window else { return failed("The window is closing.") };
                let Some(tab) = w.tabs.iter_mut().find(|t| t.pane(pane).is_some()) else { return failed("That pane is gone.") };
                tab.custom_title = (!title.is_empty()).then_some(title);
                w.update_title();
                w.window.request_redraw();
                (0, String::new())
            }
            "action" => {
                let name = parsed.words.join(" ");
                let Some(action) = Action::parse(&name) else {
                    return bad(format!("Unknown action: {name:?}. They're the names keybindings use, like new_tab or toggle_zoom."));
                };
                // Actions work on the active pane: an explicit one is shown first.
                if parsed.has(&["-p", "--pane"]) {
                    self.focus_pane(pane);
                }
                self.perform(action);
                (0, String::new())
            }
            _ => bad(remote::USAGE),
        }
    }

    /// The pane a command is about: `-p`, else the caller's, else the active one.
    fn remote_target(&self, caller: Option<PaneId>, explicit: Option<&str>) -> Result<PaneId, Answer> {
        let w = self.window.as_ref().ok_or_else(|| failed("The window is closing."))?;
        let exists = |id: PaneId| w.tabs.iter().any(|t| t.pane(id).is_some());
        if let Some(text) = explicit {
            return match text.parse() {
                Ok(id) if exists(id) => Ok(id),
                _ => Err(bad(format!("No pane {text}. \"ionterm @ ls\" lists them."))),
            };
        }
        Ok(caller.filter(|&id| exists(id)).unwrap_or_else(|| w.active_pane().id))
    }

    /// One line per pane: window and tab number, pane, `*` for each
    /// window's focused pane, its directory and title, tab-separated for
    /// scripts.
    fn remote_list(&self) -> String {
        let mut out = String::from("window\ttab\tpane\tactive\tcwd\ttitle\n");
        for (number, id) in self.window_order().into_iter().enumerate() {
            let Some(w) = self.windows().find(|w| w.id == id) else { continue };
            let infos = w.tab_infos(&self.config);
            for (index, tab) in w.tabs.iter().enumerate() {
                for pane in &tab.panes {
                    let active = index == w.active_tab && pane.id == tab.active;
                    let cwd = match (&pane.remote_cwd, pane.working_dir()) {
                        (Some(remote), _) if pane.connection.is_some() || pane.detected_ssh.is_some() => remote.clone(),
                        (_, Some(dir)) => dir.display().to_string(),
                        _ => String::new(),
                    };
                    let title = match (&pane.title, &pane.process) {
                        (title, _) if !title.is_empty() => title.clone(),
                        (_, Some(process)) => process.clone(),
                        _ => infos[index].title.clone(),
                    };
                    let mark = if active { "*" } else { "" };
                    out.push_str(&format!("{}\t{}\t{}\t{mark}\t{cwd}\t{title}\n", number + 1, index + 1, pane.id));
                }
            }
        }
        out
    }

    /// `new-window [--cwd DIR] [--title T] [--activation-token T]
    /// [--host HOST | --ssh SSH-ARGS... | command...]`, answered once the
    /// window is open.
    fn remote_new_window(&mut self, request: Request) {
        let args = &request.args[1..];
        // ssh's own options aren't ours to parse.
        let (ours, ssh_args) = match args.iter().position(|a| a == "--ssh") {
            Some(at) => (&args[..at], Some(&args[at + 1..])),
            None => (args, None),
        };
        let parsed = match Parsed::parse(ours, &[], &["--cwd", "--title", "--host", "--activation-token"]) {
            Ok(parsed) => parsed,
            Err(message) => return request.reply(BAD_ARGUMENTS, message),
        };
        let cwd = parsed.value(&["--cwd"]).map(PathBuf::from);
        let content = match (parsed.value(&["--host"]), ssh_args) {
            (Some(_), Some(_)) => return request.reply(BAD_ARGUMENTS, "Pick one of --host and --ssh."),
            (Some(host), None) => Content::Host(host.to_string()),
            (None, Some(ssh_args)) => match ssh::detect::parse_args(ssh_args) {
                Ok(parsed) => Content::Ssh(parsed),
                Err(message) => {
                    return request.reply(BAD_ARGUMENTS, format!("{message} Usage: ionterm @ new-window --ssh [ssh options] <destination>"));
                }
            },
            (None, None) if parsed.words.is_empty() => Content::Shell { cwd },
            (None, None) => Content::Command { argv: parsed.words.clone(), cwd },
        };
        let title = parsed.value(&["--title"]).map(str::to_string);
        let activation_token = parsed.value(&["--activation-token"]).map(str::to_string);
        self.request_window(NewWindow {
            title,
            activation_token,
            reply: Some(request),
            ..NewWindow::new(content)
        });
    }

    /// The window numbered `text` in `ls`.
    fn window_numbered(&self, text: &str) -> Result<winit::window::WindowId, Answer> {
        let order = self.window_order();
        text.parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| order.get(n).copied())
            .ok_or_else(|| bad(format!("No window {text}. \"ionterm @ ls\" lists them.")))
    }

    fn remote_move_tab(&mut self, pane: PaneId, parsed: &Parsed) -> Answer {
        let to = match (parsed.has(&["--new-window"]), parsed.value(&["--window"])) {
            (true, None) => None,
            (false, Some(number)) => match self.window_numbered(number) {
                Ok(id) => Some(id),
                Err(answer) => return answer,
            },
            _ => return bad("Usage: ionterm @ move-tab [-p PANE] (--new-window | --window N)"),
        };
        let Some(w) = &self.window else { return failed("The window is closing.") };
        let Some(index) = w.tabs.iter().position(|t| t.pane(pane).is_some()) else { return failed("That pane is gone.") };
        if to == Some(w.id) {
            return (0, String::new());
        }
        self.move_tab(index, to);
        (0, String::new())
    }

    fn remote_move_pane(&mut self, pane: PaneId, parsed: &Parsed) -> Answer {
        const USAGE: &str = "Usage: ionterm @ move-pane [-p PANE] (--new-tab | --new-window | --tab N [--window N])";
        let target = match (parsed.has(&["--new-tab"]), parsed.has(&["--new-window"]), parsed.value(&["--tab"])) {
            (true, false, None) => PaneTarget::NewTab,
            (false, true, None) => PaneTarget::NewWindow,
            (false, false, Some(number)) => {
                // Tab N of window N, or of the pane's own window.
                let window = match parsed.value(&["--window"]) {
                    Some(text) => match self.window_numbered(text) {
                        Ok(id) => Some(id),
                        Err(answer) => return answer,
                    },
                    None => self.window.as_ref().map(|w| w.id),
                };
                let tab = self
                    .windows()
                    .find(|w| Some(w.id) == window)
                    .and_then(|w| number.parse::<usize>().ok().and_then(|n| n.checked_sub(1)).and_then(|n| w.tabs.get(n)))
                    .map(|t| t.id);
                match tab {
                    Some(tab) => PaneTarget::Tab(tab),
                    None => return bad(format!("No tab {number} there. \"ionterm @ ls\" lists them.")),
                }
            }
            _ => return bad(USAGE),
        };
        self.move_pane(pane, target);
        (0, String::new())
    }

    fn remote_send_text(&mut self, id: PaneId, text: &str) -> Answer {
        let Some(w) = &self.window else { return failed("The window is closing.") };
        let Some(pane) = w.tabs.iter().find_map(|t| t.pane(id)) else { return failed("That pane is gone.") };
        pane.write(text.as_bytes());
        pane.lock().scroll_to_bottom();
        (0, String::new())
    }

    fn remote_get_text(&self, id: PaneId, all: bool) -> Answer {
        let Some(w) = &self.window else { return failed("The window is closing.") };
        let Some(pane) = w.tabs.iter().find_map(|t| t.pane(id)) else { return failed("That pane is gone.") };
        let term = pane.lock();
        let first = if all { 0 } else { term.viewport_to_absolute(0) };
        let last = if all { term.history_len() + term.rows() - 1 } else { term.viewport_to_absolute(term.rows() - 1) };
        let text = term.text_between((first, 0), (last, term.cols() - 1));
        (0, format!("{}\n", text.trim_end_matches('\n')))
    }

    fn remote_new_tab(&mut self, command: Vec<String>, cwd: Option<PathBuf>, title: Option<String>) -> Answer {
        let Some(w) = &self.window else { return failed("The window is closing.") };
        let end = w.tabs.len();
        let cwd = cwd.or_else(|| w.active_pane().working_dir());
        if command.is_empty() {
            self.open_tab(end, None, cwd);
        } else {
            self.open_command_tab_at(end, command, cwd);
        }
        let Some(w) = &mut self.window else { return failed("The window is closing.") };
        if w.tabs.len() == end {
            return failed("Couldn't open the tab (see ionterm's log).");
        }
        let tab = &mut w.tabs[end];
        tab.custom_title = title.filter(|t| !t.is_empty());
        (0, format!("{}\n", tab.active))
    }

    /// Opens a tab connected to a config host (`host`) or with ssh
    /// arguments (`ssh`), and brings the window forward.
    fn remote_connect(&mut self, command: &str, args: &[String]) -> Answer {
        let connection = if command == "host" {
            let [alias] = args else { return bad("Usage: ionterm @ host <host>") };
            self.connection_for(alias)
        } else {
            match ssh::detect::parse_args(args) {
                Ok(parsed) => self.connection_from_args(&parsed),
                Err(message) => return bad(format!("{message} Usage: ionterm @ ssh [ssh options] <destination> [command]")),
            }
        };
        let Some(w) = &self.window else { return failed("The window is closing.") };
        let end = w.tabs.len();
        self.open_tab(end, Some(connection), None);
        let Some(w) = &self.window else { return failed("The window is closing.") };
        if w.tabs.len() == end {
            return failed("Couldn't open the tab (see ionterm's log).");
        }
        w.window.focus_window();
        (0, format!("{}\n", w.tabs[end].active))
    }

    fn remote_split(&mut self, id: PaneId, direction: Option<Direction>, command: Vec<String>, cwd: Option<PathBuf>) -> Answer {
        self.focus_pane(id);
        let Some(w) = &self.window else { return failed("The window is closing.") };
        let before = w.active_pane().id;
        let cwd = cwd.or_else(|| w.active_pane().working_dir());
        let command = (!command.is_empty()).then_some(command);
        self.split_running(direction, command, cwd);
        let Some(w) = &self.window else { return failed("The window is closing.") };
        match w.active_pane().id {
            new if new != before => (0, format!("{new}\n")),
            _ => failed("Couldn't split (see ionterm's log)."),
        }
    }

    /// Shows a pane's tab and makes it the active pane.
    fn focus_pane(&mut self, id: PaneId) {
        let Some(w) = &mut self.window else { return };
        let Some(index) = w.tabs.iter().position(|t| t.pane(id).is_some()) else { return };
        w.select_tab(index);
        w.set_active_pane(id);
        w.window.request_redraw();
    }
}
