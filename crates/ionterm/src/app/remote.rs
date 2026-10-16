//! Remote control commands (`ionterm @ …`), run on the event loop: see
//! `crate::remote` for the socket and the client.

use std::path::PathBuf;

use super::App;
use crate::input::Action;
use crate::layout::{Direction, PaneId};
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

    /// One line per pane: tab number, pane, `*` for the focused one, its
    /// directory and title, tab-separated for scripts.
    fn remote_list(&self) -> String {
        let Some(w) = &self.window else { return String::new() };
        let infos = w.tab_infos(&self.config);
        let mut out = String::from("tab\tpane\tactive\tcwd\ttitle\n");
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
                out.push_str(&format!("{}\t{}\t{}\t{cwd}\t{title}\n", index + 1, pane.id, if active { "*" } else { "" }));
            }
        }
        out
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
