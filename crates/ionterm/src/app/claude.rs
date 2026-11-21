//! Claude Code in the app: hook reports onto panes, the marks and
//! notifications they cause, the agents popup, "Ask Claude", installing the
//! plugin, and the pane tools the plugin's MCP server calls (`claude-panes`,
//! `claude-read`, `claude-run`, `claude-job`, `claude-cancel`). See
//! `crate::claude` for the parts that need no window.

use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ionterm_vt::TermMode;
use serde_json::json;
use winit::event_loop::EventLoopProxy;

use super::{App, TermWindow, UserEvent};
use crate::claude::{self, Agent, Event, Status, Transition};
use crate::dialog::{self, Dialog};
use crate::layout::{Direction, PaneId};
use crate::notifications;
use crate::palette::{Palette, PaletteAction, PaletteItem};
use crate::pane::Pane;
use crate::remote::BAD_ARGUMENTS;
use crate::tabbar::Activity;

type Answer = (u8, String);

/// The most output handed to Claude at once: the end of it.
const MAX_OUTPUT: usize = 60_000;
/// Finished jobs are forgotten after this.
const JOB_KEPT: Duration = Duration::from_secs(600);
/// How long "Ask Claude" waits for a Claude it started to be ready.
const PASTE_WAIT: Duration = Duration::from_secs(20);
/// Claude's own orange, for the mark on shared tabs.
pub const CLAUDE_COLOR: ionterm_vt::Rgb = ionterm_vt::Rgb { r: 0xd9, g: 0x77, b: 0x57 };
/// The icon for a pane running Claude Code (nf-md-robot_outline).
pub(super) const CLAUDE_ICON: char = '\u{F167A}';
/// The icon for an agent waiting for you (nf-md-bell_ring).
pub(super) const WAITING_ICON: char = '\u{F009E}';

/// Background work finishing.
#[derive(Debug)]
pub enum ClaudeEvent {
    /// Installing or updating the plugin: what it did, or why it failed.
    Plugin(Result<Option<String>, String>),
    /// A separately run command ended: its exit code, or why it couldn't run.
    ExecDone { job: u64, result: Result<i32, String> },
}

/// Claude Code state that belongs to no single window.
#[derive(Default)]
pub(super) struct Claude {
    jobs: Vec<Job>,
    next_job: u64,
    /// Installing the plugin is under way.
    installing: bool,
    /// Commands from (target pane, Claude's pane, its session) that run
    /// without asking, after "Allow all".
    allowed: HashSet<(PaneId, PaneId, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Typed into the pane's shell.
    Shell,
    /// Run beside it, over the same connection.
    Exec,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Shell => "typed into the shell",
            Mode::Exec => "run separately",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum JobState {
    /// Waiting for you to allow it; `asked` once the dialog showed.
    Approval { asked: bool },
    /// Typed in, waiting for the shell to start it (`started` once it did).
    Running { started: bool },
    Done(Option<i32>),
    Denied,
    Cancelled,
    Failed(String),
}

struct Job {
    id: u64,
    pane: PaneId,
    /// The pane Claude runs in, and its session.
    requester: Option<(PaneId, String)>,
    command: String,
    mode: Mode,
    state: JobState,
    /// Exec output as it arrives; shell output is read from the pane.
    output: Arc<Mutex<String>>,
    /// The exec's process, to stop it.
    child: Option<Arc<Mutex<std::process::Child>>>,
    ended: Option<Instant>,
}

impl App {
    //#region Commands from the plugin

    /// `claude-*` remote commands: hook reports and the MCP server's tools.
    pub(super) fn claude_command(&mut self, caller: Option<PaneId>, command: &str, args: &[String]) -> Answer {
        match command {
            "claude-event" => self.claude_event(caller, args),
            "claude-panes" => self.claude_panes(),
            "claude-agents" => (0, self.claude_agents()),
            "claude-read" => match args {
                [pane, what, rest @ ..] => self.claude_read(pane, what, rest.first().map(String::as_str)),
                _ => (BAD_ARGUMENTS, "Usage: claude-read PANE screen|lines N|command N".into()),
            },
            "claude-run" => match args {
                [pane, mode, command] => self.claude_run(caller, pane, mode, command),
                _ => (BAD_ARGUMENTS, "Usage: claude-run PANE auto|shell|exec COMMAND".into()),
            },
            "claude-job" => match args.first().and_then(|j| j.parse().ok()) {
                Some(job) => self.claude_job(job),
                None => (BAD_ARGUMENTS, "Usage: claude-job JOB".into()),
            },
            "claude-cancel" => match args.first().and_then(|j| j.parse().ok()) {
                Some(job) => self.claude_cancel(job),
                None => (BAD_ARGUMENTS, "Usage: claude-cancel JOB".into()),
            },
            other => (BAD_ARGUMENTS, format!("Unknown command: {other}.")),
        }
    }

    fn tracking(&self) -> bool {
        let c = &self.config.claude;
        c.status || c.overview || c.ask || c.panes
    }

    /// A hook's report about the Claude running in the caller's pane.
    fn claude_event(&mut self, caller: Option<PaneId>, words: &[String]) -> Answer {
        if !self.tracking() {
            return (0, String::new());
        }
        let event = match Event::parse(words) {
            Ok(event) => event,
            Err(message) => return (BAD_ARGUMENTS, message),
        };
        let Some(id) = caller else { return (BAD_ARGUMENTS, "Not run in an ionterm pane.".into()) };
        if !self.select_window_of_pane(id) {
            return (1, "That pane is gone.".into());
        }
        let config = self.config.clone();
        let Some(w) = &mut self.window else { return (1, "The window is closing.".into()) };
        let Some(tab_index) = w.tabs.iter().position(|t| t.pane(id).is_some()) else { return (1, "That pane is gone.".into()) };
        let now = Instant::now();
        let elsewhere = tab_index != w.active_tab || !w.tabs[tab_index].is_visible(id);
        let focused = w.focused;
        let pane = w.tabs[tab_index].pane_mut(id).expect("found above");
        let transition = Agent::apply(&mut pane.claude, &event, now);
        // A Claude that "Ask Claude" started is ready for its text.
        if event.name == "SessionStart"
            && let Some((text, _)) = pane.claude_paste.take()
        {
            let mode = pane.lock().mode();
            pane.write(&crate::input::encode_paste(&text, mode));
        }
        let Some(agent) = pane.claude.clone() else {
            w.window.request_redraw();
            return (0, String::new());
        };
        let notify = config.claude.status && config.claude.notify && (elsewhere || !focused);
        match transition {
            Transition::NeedsYou => {
                if !focused {
                    w.window.request_user_attention(Some(winit::window::UserAttentionType::Informational));
                }
                if notify {
                    let why = agent.attention.clone().unwrap_or_else(|| "Claude needs your input".into());
                    notifications::send(&format!("Claude needs you · {}", agent.project()), &why);
                }
            }
            Transition::Finished(took) => {
                if elsewhere && config.claude.status {
                    w.tabs[tab_index].activity = Activity::Done(!agent.failed);
                }
                let long_after = config.notifications.command_done_after;
                if notify && long_after > 0.0 && took.as_secs_f32() >= long_after {
                    let mark = if agent.failed { "✗" } else { "✓" };
                    let what = agent.last_message.clone().or(agent.prompt.clone()).unwrap_or_default();
                    notifications::send(
                        &format!("{mark} Claude finished · {}", agent.project()),
                        &format!("{} (after {})", claude::clip(&what, 160), notifications::duration(took)),
                    );
                }
            }
            _ => {}
        }
        // The tab's icon, a mark or the agents popup may show the change.
        w.window.request_redraw();
        (0, String::new())
    }

    /// One line per Claude session: pane, state, project, what it's doing.
    fn claude_agents(&self) -> String {
        let now = Instant::now();
        let mut out = String::from("pane\tstate\tproject\tshared\tdoing\n");
        for w in self.windows() {
            for pane in w.tabs.iter().flat_map(|t| &t.panes) {
                let Some(agent) = &pane.claude else { continue };
                let state = match agent.status {
                    Status::Idle => "idle",
                    Status::Working => "working",
                    Status::NeedsYou => "needs-you",
                };
                out.push_str(&format!("{}\t{state}\t{}\t{}\t{}\n", pane.id, agent.project(), pane.claude_shared, agent.describe(now)));
            }
        }
        out
    }

    //#endregion

    //#region Pane tools

    /// The panes shared with Claude, as JSON for its tools.
    fn claude_panes(&self) -> Answer {
        if !self.config.claude.panes {
            return (1, "Pane access is off in ionterm (Settings › Claude Code › Pane access).".into());
        }
        let mut list = Vec::new();
        for w in self.windows() {
            for tab in &w.tabs {
                for pane in tab.panes.iter().filter(|p| p.claude_shared) {
                    let term = pane.lock();
                    let last = term.prompt_lines().into_iter().rev().filter_map(|p| crate::blocks::block_at(&term, p)).find(|b| b.finished);
                    list.push(json!({
                        "pane": pane.id,
                        "title": tab.custom_title.clone().unwrap_or_else(|| pane.title.clone()),
                        "host": pane_host(pane),
                        "cwd": pane.remote_cwd.clone().or_else(|| pane.working_dir().map(|d| d.display().to_string())),
                        "shell_idle": shell_idle(pane, &term),
                        "exec": exec_target(pane).is_some(),
                        "last_command": last.as_ref().and_then(|b| b.command.clone()),
                        "last_exit": last.and_then(|b| b.status),
                    }));
                }
            }
        }
        if list.is_empty() {
            return (0, "No panes are shared with you. Ask the user to pick \"Share with Claude\" on the tab or pane you should use (right-click the tab, or the command palette).".into());
        }
        (0, serde_json::to_string_pretty(&list).unwrap_or_default())
    }

    /// A shared pane, in any window.
    fn shared_pane(&self, text: &str) -> Result<&Pane, Answer> {
        if !self.config.claude.panes {
            return Err((1, "Pane access is off in ionterm (Settings › Claude Code › Pane access).".into()));
        }
        let id: PaneId = text.parse().map_err(|_| (BAD_ARGUMENTS, format!("{text:?} isn't a pane id.")))?;
        self.windows()
            .flat_map(|w| w.tabs.iter())
            .find_map(|t| t.pane(id))
            .filter(|p| p.claude_shared)
            .ok_or_else(|| (1, format!("Pane {id} isn't shared with you (or is gone). list_panes shows the ones that are.")))
    }

    fn claude_read(&self, pane: &str, what: &str, n: Option<&str>) -> Answer {
        let pane = match self.shared_pane(pane) {
            Ok(pane) => pane,
            Err(answer) => return answer,
        };
        let term = pane.lock();
        let last_col = term.cols().saturating_sub(1);
        let total = term.history_len() + term.rows();
        let text = match what {
            "lines" => {
                let lines: usize = n.and_then(|n| n.parse().ok()).unwrap_or(200).clamp(1, 5000);
                term.text_between((total.saturating_sub(lines), 0), (total - 1, last_col))
            }
            "command" => {
                let back: usize = n.and_then(|n| n.parse().ok()).unwrap_or(0);
                let blocks: Vec<_> = term.prompt_lines().into_iter().rev().filter_map(|p| crate::blocks::block_at(&term, p)).filter(|b| b.finished).collect();
                let Some(block) = blocks.get(back) else {
                    return (1, "No such command: this shell has no shell integration, or fewer commands ran.".into());
                };
                let output = block.output.map(|(s, e)| term.text_between((s, 0), (e, last_col))).unwrap_or_default();
                let status = block.status.map_or("unknown".into(), |s| s.to_string());
                format!("$ {}\n(exit code {status})\n{output}", block.command.as_deref().unwrap_or("?"))
            }
            _ => {
                let first = term.viewport_to_absolute(0);
                let last = term.viewport_to_absolute(term.rows() - 1);
                term.text_between((first, 0), (last, last_col))
            }
        };
        (0, tail(text.trim_end()))
    }

    /// Starts a command for Claude, or asks you first. Answers with its job.
    fn claude_run(&mut self, caller: Option<PaneId>, pane: &str, mode: &str, command: &str) -> Answer {
        let (id, mode) = {
            let p = match self.shared_pane(pane) {
                Ok(p) => p,
                Err(answer) => return answer,
            };
            let idle = shell_idle(p, &p.lock());
            let exec = exec_target(p).is_some();
            let mode = match mode {
                "shell" if !idle => return (1, "The shell in that pane is busy (a command or a full-screen program is running). Try mode \"exec\".".into()),
                "shell" => Mode::Shell,
                "exec" if !exec => return (1, "That pane can't run commands separately (an ssh typed by hand has no shared connection). Use mode \"shell\" while it's idle.".into()),
                "exec" => Mode::Exec,
                "auto" | "" if idle => Mode::Shell,
                "auto" | "" if exec => Mode::Exec,
                "auto" | "" => return (1, "The shell in that pane is busy and it has no shared connection to run things beside it. Try again when its prompt is back.".into()),
                other => return (BAD_ARGUMENTS, format!("Unknown mode {other:?}: auto, shell or exec.")),
            };
            (p.id, mode)
        };
        let requester = caller.map(|c| {
            let session = self.windows().flat_map(|w| w.tabs.iter()).find_map(|t| t.pane(c)).and_then(|p| p.claude.as_ref()).map(|a| a.session.clone());
            (c, session.unwrap_or_default())
        });
        let allowed = requester.as_ref().is_some_and(|(c, s)| self.claude.allowed.contains(&(id, *c, s.clone())));
        let free = allowed || claude::is_read_only(command, &self.config.claude.read_only);
        self.claude.next_job += 1;
        let job = Job {
            id: self.claude.next_job,
            pane: id,
            requester,
            command: command.to_string(),
            mode,
            state: JobState::Approval { asked: false },
            output: Arc::new(Mutex::new(String::new())),
            child: None,
            ended: None,
        };
        let number = job.id;
        self.claude.jobs.push(job);
        if free {
            self.start_job(number);
        }
        else {
            self.ask_about_jobs();
        }
        (0, json!({"job": number}).to_string())
    }

    fn claude_job(&self, number: u64) -> Answer {
        let Some(job) = self.claude.jobs.iter().find(|j| j.id == number) else { return (1, format!("No job {number}.")) };
        let output = match (&job.state, job.mode) {
            // A shell command's output so far is its block in the pane.
            (JobState::Running { started: true }, Mode::Shell) => self.running_output(job.pane).unwrap_or_default(),
            _ => job.output.lock().map(|o| o.clone()).unwrap_or_default(),
        };
        let (state, exit, error) = match &job.state {
            JobState::Approval { .. } => ("approval", None, None),
            JobState::Running { .. } => ("running", None, None),
            JobState::Done(code) => ("done", *code, None),
            JobState::Denied => ("denied", None, None),
            JobState::Cancelled => ("cancelled", None, None),
            JobState::Failed(why) => ("failed", None, Some(why.clone())),
        };
        (0, json!({"state": state, "exit": exit, "error": error, "mode": job.mode.name(), "output": tail(output.trim_end())}).to_string())
    }

    fn claude_cancel(&mut self, number: u64) -> Answer {
        let Some(job) = self.claude.jobs.iter_mut().find(|j| j.id == number) else { return (1, format!("No job {number}.")) };
        match job.state {
            JobState::Approval { .. } => job.state = JobState::Cancelled,
            JobState::Running { .. } => match job.mode {
                Mode::Exec => {
                    if let Some(child) = &job.child {
                        let _ = child.lock().map(|mut c| c.kill());
                    }
                    job.state = JobState::Cancelled;
                }
                Mode::Shell => {
                    let pane = job.pane;
                    if let Some(p) = self.windows().flat_map(|w| w.tabs.iter()).find_map(|t| t.pane(pane)) {
                        p.write(b"\x03");
                    }
                    // Done when the shell's prompt comes back.
                    return (0, "Sent ctrl+c.".into());
                }
            },
            _ => return (0, "It had already ended.".into()),
        }
        job.ended.get_or_insert_with(Instant::now);
        (0, "Cancelled.".into())
    }

    /// The output of the command running in a pane so far.
    fn running_output(&self, pane: PaneId) -> Option<String> {
        let p = self.windows().flat_map(|w| w.tabs.iter()).find_map(|t| t.pane(pane))?;
        let term = p.lock();
        let block = crate::blocks::current_block(&term, true)?;
        let (start, end) = block.output?;
        Some(term.text_between((start, 0), (end, term.cols().saturating_sub(1))))
    }

    /// Runs an allowed job.
    fn start_job(&mut self, number: u64) {
        let Some(index) = self.claude.jobs.iter().position(|j| j.id == number) else { return };
        let (pane, mode, command) = {
            let job = &self.claude.jobs[index];
            (job.pane, job.mode, job.command.clone())
        };
        let Some(p) = self.windows().flat_map(|w| w.tabs.iter()).find_map(|t| t.pane(pane)) else {
            self.claude.jobs[index].state = JobState::Failed("The pane closed.".into());
            return;
        };
        match mode {
            Mode::Shell => {
                // The shell may have got busy while you were asked.
                if !shell_idle(p, &p.lock()) {
                    self.claude.jobs[index].state = JobState::Failed("The shell got busy before the command could be typed. Try mode \"exec\".".into());
                    return;
                }
                // ctrl+e ctrl+u clears whatever is half typed first.
                let mut bytes = b"\x05\x15".to_vec();
                bytes.extend_from_slice(command.as_bytes());
                bytes.push(b'\r');
                p.write(&bytes);
                p.lock().scroll_to_bottom();
                self.claude.jobs[index].state = JobState::Running { started: false };
            }
            Mode::Exec => {
                let Some(target) = exec_target(p) else {
                    self.claude.jobs[index].state = JobState::Failed("That pane can't run commands separately.".into());
                    return;
                };
                let output = self.claude.jobs[index].output.clone();
                match spawn_exec(target, &command, output, number, self.proxy.clone()) {
                    Ok(child) => {
                        let job = &mut self.claude.jobs[index];
                        job.child = Some(child);
                        job.state = JobState::Running { started: true };
                    }
                    Err(why) => self.claude.jobs[index].state = JobState::Failed(why),
                }
            }
        }
        if let Some(w) = &self.window {
            w.window.request_redraw();
        }
    }

    /// The shell in a pane started or finished a command: shell jobs follow it.
    pub(super) fn claude_shell_event(&mut self, pane: PaneId, finished: Option<Option<i32>>) {
        let running = self.claude.jobs.iter().position(|j| j.pane == pane && j.mode == Mode::Shell && matches!(j.state, JobState::Running { .. }));
        let Some(index) = running else { return };
        match finished {
            None => self.claude.jobs[index].state = JobState::Running { started: true },
            Some(status) => {
                let output = self.claude_last_output(pane).unwrap_or_default();
                let job = &mut self.claude.jobs[index];
                if let Ok(mut buffer) = job.output.lock() {
                    *buffer = output;
                }
                job.state = JobState::Done(status);
                job.ended = Some(Instant::now());
            }
        }
    }

    fn claude_last_output(&self, pane: PaneId) -> Option<String> {
        let p = self.windows().flat_map(|w| w.tabs.iter()).find_map(|t| t.pane(pane))?;
        let term = p.lock();
        let (start, end) = term.last_command_output()?;
        Some(term.text_between((start, 0), (end, term.cols().saturating_sub(1))))
    }

    pub(super) fn claude_background(&mut self, event: ClaudeEvent) {
        match event {
            ClaudeEvent::Plugin(result) => {
                self.claude.installing = false;
                let message = match result {
                    Ok(Some(done)) => done,
                    Ok(None) => return,
                    Err(why) => format!("Claude Code plugin: {why}"),
                };
                if let Some(w) = &mut self.window {
                    w.show_toast(message);
                }
            }
            ClaudeEvent::ExecDone { job, result } => {
                let Some(job) = self.claude.jobs.iter_mut().find(|j| j.id == job) else { return };
                if job.state != JobState::Cancelled {
                    job.state = match result {
                        Ok(code) => JobState::Done(Some(code)),
                        Err(why) => JobState::Failed(why),
                    };
                }
                job.child = None;
                job.ended = Some(Instant::now());
            }
        }
    }

    /// Opens the approval dialog for the next job that needs one, in its
    /// pane's window, once that window has no other dialog up.
    pub(super) fn ask_about_jobs(&mut self) {
        let Some(number) = self.claude.jobs.iter().find(|j| j.state == JobState::Approval { asked: false }).map(|j| j.id) else { return };
        let pane = self.claude.jobs.iter().find(|j| j.id == number).map(|j| j.pane).unwrap_or_default();
        let current = self.window.as_ref().map(|w| w.id);
        if !self.select_window_of_pane(pane) {
            if let Some(job) = self.claude.jobs.iter_mut().find(|j| j.id == number) {
                job.state = JobState::Failed("The pane closed.".into());
            }
            return;
        }
        let Some(job) = self.claude.jobs.iter_mut().find(|j| j.id == number) else { return };
        let Some(w) = &mut self.window else { return };
        if w.dialog.is_none() {
            let place = w.tabs.iter().find_map(|t| t.pane(pane)).map(pane_host).unwrap_or_else(|| "this pane".into());
            let mut open = Dialog::new(
                "Let Claude run this?",
                format!("On {place}, {}:\n\n{}", job.mode.name(), job.command),
                vec![
                    dialog::Button::new("Run", 'y').danger(),
                    dialog::Button::new("Allow all from this session", 'a'),
                    dialog::Button::new("Don't run", 'n'),
                ],
                super::files::Purpose::ClaudeRun(number),
            );
            open.default = 2;
            // Nothing else open over it or taking its keys.
            w.palette = None;
            w.menu = None;
            w.dialog = Some(open);
            job.state = JobState::Approval { asked: true };
            if !w.focused {
                w.window.request_user_attention(Some(winit::window::UserAttentionType::Critical));
            }
            w.window.request_redraw();
        }
        if let Some(id) = current {
            self.select_window(id);
        }
    }

    /// Your answer to "Let Claude run this?".
    pub(super) fn claude_run_answer(&mut self, number: u64, answer: dialog::Answer) {
        let Some(job) = self.claude.jobs.iter_mut().find(|j| j.id == number) else { return };
        if job.state != (JobState::Approval { asked: true }) {
            return;
        }
        match answer.button {
            Some(0) => self.start_job(number),
            Some(1) => {
                if let Some((caller, session)) = job.requester.clone() {
                    self.claude.allowed.insert((job.pane, caller, session));
                }
                self.start_job(number);
            }
            _ => {
                job.state = JobState::Denied;
                job.ended = Some(Instant::now());
            }
        }
        self.ask_about_jobs();
    }

    //#endregion

    //#region Plugin, popup, Ask Claude

    /// Installs or updates the plugin in the background, when a feature
    /// that needs it is on.
    pub(super) fn ensure_claude_plugin(&mut self) {
        let c = &self.config.claude;
        if !(c.status || c.overview || c.panes) || self.claude.installing || !claude::available() {
            return;
        }
        self.claude.installing = true;
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new().name("claude-plugin".into()).spawn(move || {
            let _ = proxy.send_event(UserEvent::Claude(ClaudeEvent::Plugin(claude::ensure_plugin())));
        });
        if spawned.is_err() {
            self.claude.installing = false;
        }
    }

    /// Removes the plugin (from the palette).
    pub(super) fn remove_claude_plugin(&mut self) {
        let proxy = self.proxy.clone();
        let _ = std::thread::Builder::new().name("claude-plugin".into()).spawn(move || {
            let result = claude::remove_plugin().map(|()| Some("Removed the ionterm plugin from Claude Code.".to_string()));
            let _ = proxy.send_event(UserEvent::Claude(ClaudeEvent::Plugin(result)));
        });
    }

    /// The agents popup: every Claude session in every window, those that
    /// need you first.
    pub(super) fn open_agents(&mut self) {
        if !self.config.claude.overview {
            if let Some(w) = &mut self.window {
                w.show_toast("The agents overview is off: turn it on in Settings › Claude Code");
            }
            return;
        }
        let now = Instant::now();
        let mut agents: Vec<(Status, Instant, PaletteItem)> = Vec::new();
        for (number, id) in self.window_order().into_iter().enumerate() {
            let Some(w) = self.windows().find(|w| w.id == id) else { continue };
            for tab in &w.tabs {
                for pane in &tab.panes {
                    let Some(agent) = &pane.claude else { continue };
                    let (group, icon, color) = match agent.status {
                        Status::NeedsYou => ("Needs you", WAITING_ICON, Some(self.theme.palette().colors[3])),
                        Status::Working => ("Working", claude::spinner(now), None),
                        Status::Idle => ("Waiting for a prompt", CLAUDE_ICON, None),
                    };
                    let what = agent.title.clone().or(agent.prompt.clone()).unwrap_or_else(|| "New session".into());
                    let place = if self.parked.is_empty() { String::new() } else { format!(" · window {}", number + 1) };
                    agents.push((
                        agent.status,
                        agent.since,
                        PaletteItem {
                            title: format!("{} — {}", agent.project(), claude::clip(&what, 70)),
                            detail: format!("{}{place}", agent.describe(now)),
                            group: group.into(),
                            icon,
                            color,
                            action: PaletteAction::FocusPane(pane.id),
                        },
                    ));
                }
            }
        }
        let rank = |s: Status| match s {
            Status::NeedsYou => 0,
            Status::Working => 1,
            Status::Idle => 2,
        };
        // Longest waiting first within each group.
        agents.sort_by_key(|(status, since, _)| (rank(*status), *since));
        let mut items: Vec<PaletteItem> = agents.into_iter().map(|(_, _, item)| item).collect();
        items.push(PaletteItem {
            title: "New Claude session".into(),
            detail: "In a new tab, in this directory".into(),
            group: String::new(),
            icon: CLAUDE_ICON,
            color: None,
            action: PaletteAction::NewClaude,
        });
        let Some(w) = &mut self.window else { return };
        w.palette = Some(Palette::chooser(items, AGENTS_PLACEHOLDER, "Enter jump to it · Esc close"));
        w.window.request_redraw();
    }

    /// Shows a pane in whichever window has it, and brings that window up.
    pub(super) fn show_pane_anywhere(&mut self, id: PaneId) {
        if !self.select_window_of_pane(id) {
            return;
        }
        let Some(w) = &mut self.window else { return };
        let Some(index) = w.tabs.iter().position(|t| t.pane(id).is_some()) else { return };
        w.select_tab(index);
        w.set_active_pane(id);
        w.window.focus_window();
        w.window.request_redraw();
    }

    /// Jumps to the next agent that needs you (after the one shown), or else
    /// the one that finished longest ago.
    pub(super) fn next_waiting_agent(&mut self) {
        if !self.config.claude.overview {
            if let Some(w) = &mut self.window {
                w.show_toast("The agents overview is off: turn it on in Settings › Claude Code");
            }
            return;
        }
        let current = self.window.as_ref().map(|w| w.active_pane().id);
        let mut waiting: Vec<(PaneId, Status, Instant)> = Vec::new();
        for id in self.window_order() {
            let Some(w) = self.windows().find(|w| w.id == id) else { continue };
            for pane in w.tabs.iter().flat_map(|t| &t.panes) {
                if let Some(agent) = pane.claude.as_ref().filter(|a| a.status != Status::Working) {
                    waiting.push((pane.id, agent.status, agent.since));
                }
            }
        }
        let needs: Vec<PaneId> = waiting.iter().filter(|w| w.1 == Status::NeedsYou).map(|w| w.0).collect();
        let pick = if needs.is_empty() {
            waiting.iter().filter(|w| Some(w.0) != current).min_by_key(|w| w.2).map(|w| w.0)
        }
        else {
            let after = current.and_then(|c| needs.iter().position(|&n| n == c)).map_or(0, |i| i + 1);
            Some(needs[after % needs.len()])
        };
        match pick {
            Some(id) => self.show_pane_anywhere(id),
            None => {
                if let Some(w) = &mut self.window {
                    w.show_toast("No Claude session is waiting for you");
                }
            }
        }
    }

    /// Opens a tab running `claude` in the active pane's directory.
    pub(super) fn new_claude_tab(&mut self) {
        let Some(w) = &self.window else { return };
        let end = w.tabs.len();
        let cwd = w.active_pane().working_dir();
        self.open_tab(end, None, cwd);
        let Some(w) = &self.window else { return };
        if let Some(tab) = w.tabs.get(end) {
            // Typed ahead into the new shell, so quitting Claude leaves you there.
            tab.active_pane().write(b"claude\r");
        }
    }

    /// "Ask Claude" about text: pasted into the Claude in this tab when it
    /// waits at its prompt, else into a new split running `claude`. Never
    /// sent: you add the question.
    pub(super) fn ask_claude(&mut self, text: String) {
        if !self.config.claude.ask {
            if let Some(w) = &mut self.window {
                w.show_toast("Ask Claude is off: turn it on in Settings › Claude Code");
            }
            return;
        }
        let Some(w) = &mut self.window else { return };
        let tab = &w.tabs[w.active_tab];
        let waiting = tab.panes.iter().find(|p| p.claude.as_ref().is_some_and(|a| a.status == Status::Idle) && !at_shell(p));
        if let Some(target) = waiting.map(|p| p.id) {
            w.paste_into(target, &text);
            w.set_active_pane(target);
            w.show_toast("Pasted into Claude: add your question and press Enter");
            w.window.request_redraw();
            return;
        }
        // Local: a new split is always a local shell.
        let cwd = tab.active_pane().connection.is_none().then(|| tab.active_pane().working_dir()).flatten();
        let before = w.active_pane().id;
        self.split_with(Some(Direction::Horizontal), None, cwd);
        let Some(w) = &mut self.window else { return };
        let new = w.active_pane().id;
        if new == before {
            return;
        }
        if let Some(p) = w.tabs.iter_mut().find_map(|t| t.pane_mut(new)) {
            p.write(b"claude\r");
            p.claude_paste = Some((text, Instant::now() + PASTE_WAIT));
        }
        w.show_toast("Starting Claude: the output goes into its prompt when it's ready");
    }

    /// Toggles sharing a pane with Claude's pane tools.
    pub(super) fn toggle_claude_share(&mut self) {
        let enabled = self.config.claude.panes;
        let Some(w) = &mut self.window else { return };
        if !enabled {
            return w.show_toast("Pane access is off: turn it on in Settings › Claude Code");
        }
        let tab = w.active_tab;
        let pane = w.tabs[tab].active_pane_mut();
        pane.claude_shared = !pane.claude_shared;
        let (shared, id) = (pane.claude_shared, pane.id);
        w.show_toast(if shared {
            format!("Shared pane {id} with Claude: it can read it and run commands (you're asked unless they only read)")
        }
        else {
            format!("Stopped sharing pane {id} with Claude")
        });
        w.window.request_redraw();
    }

    //#endregion
}

const AGENTS_PLACEHOLDER: &str = "Jump to a Claude session…";

impl TermWindow {
    /// Spinner frames, pending pastes and the end of stale agents. Returns
    /// when to look again.
    pub(super) fn claude_timers(&mut self, now: Instant, status: bool) -> Option<Instant> {
        let mut wake: Option<Instant> = None;
        let mut redraw = false;
        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                if pane.claude.is_none() && pane.claude_paste.is_none() {
                    continue;
                }
                // Only refreshed for the tab bar otherwise; rate-limited.
                redraw |= pane.refresh_process(now);
                // Claude quit without its SessionEnd (killed, crashed): the
                // shell is back in front.
                if pane.claude.as_ref().is_some_and(|a| now.duration_since(a.created) > STALE_GRACE) && at_shell(pane) {
                    pane.claude = None;
                    redraw = true;
                }
                if let Some((text, until)) = pane.claude_paste.take() {
                    // Without the plugin there's no SessionStart: bracketed
                    // paste switched on by Claude (not by the shell, which is
                    // no longer in front) says it's reading input.
                    let ready = pane.process.is_some() && !at_shell(pane) && pane.lock().mode().contains(TermMode::BRACKETED_PASTE);
                    if ready && pane.claude.is_none() {
                        let mode = pane.lock().mode();
                        pane.write(&crate::input::encode_paste(&text, mode));
                    }
                    else if until > now {
                        pane.claude_paste = Some((text, until));
                        wake = [wake, Some(now + Duration::from_millis(300))].into_iter().flatten().min();
                    }
                }
            }
        }
        let working = status && self.tab_bar.is_some() && self.tabs.iter().flat_map(|t| &t.panes).any(|p| p.claude.as_ref().is_some_and(|a| a.status == Status::Working));
        if working {
            redraw = true;
            wake = [wake, Some(now + claude::SPINNER_FRAME)].into_iter().flatten().min();
        }
        if redraw {
            self.window.request_redraw();
        }
        wake
    }

    /// What a tab shows for the Claude in it: its icon, whether it needs
    /// you, and its todo progress. `None` without one.
    pub(super) fn claude_tab(&self, tab: usize, now: Instant) -> Option<(char, bool, Option<f32>)> {
        let tab = &self.tabs[tab];
        // A waiting agent anywhere in the tab shows, else the active pane's.
        let agent = tab
            .panes
            .iter()
            .filter_map(|p| p.claude.as_ref())
            .find(|a| a.status == Status::NeedsYou)
            .or(tab.active_pane().claude.as_ref())
            .or_else(|| tab.panes.iter().find_map(|p| p.claude.as_ref()))?;
        let icon = match agent.status {
            Status::Working => claude::spinner(now),
            Status::NeedsYou => WAITING_ICON,
            Status::Idle => CLAUDE_ICON,
        };
        Some((icon, agent.status == Status::NeedsYou, agent.todo_fraction()))
    }

    /// Whether a pane of the tab is shared with Claude.
    pub(super) fn tab_shared(&self, tab: usize) -> bool {
        self.tabs[tab].panes.iter().any(|p| p.claude_shared)
    }
}

impl Claude {
    /// Forgets jobs that ended a while ago.
    pub(super) fn expire(&mut self, now: Instant) {
        self.jobs.retain(|j| j.ended.is_none_or(|at| now.duration_since(at) < JOB_KEPT));
    }
}

/// How long a new agent is kept even though the pane's (rate-limited)
/// foreground program still says the shell.
const STALE_GRACE: Duration = Duration::from_secs(5);

/// The pane's shell is in front: no program like Claude runs in it. (Claude
/// shows up as `claude`, or `node` when installed with npm.)
fn at_shell(pane: &Pane) -> bool {
    pane.process.as_deref().is_some_and(crate::tabbar::is_shell)
}

/// Where a pane is, for Claude and the approval dialog.
fn pane_host(pane: &Pane) -> String {
    pane.connection
        .as_ref()
        .map(|c| c.alias.clone())
        .or(pane.detected_ssh.as_ref().map(|d| d.host().to_string()))
        .unwrap_or_else(|| "this computer".into())
}

/// The shell waits at its prompt: nothing running, no full-screen program,
/// shell integration on.
fn shell_idle(pane: &Pane, term: &ionterm_vt::Term) -> bool {
    pane.command.is_none()
        && pane.claude.is_none()
        && !term.mode().contains(TermMode::ALT_SCREEN)
        && !term.prompt_lines().is_empty()
}

/// Where a separate command runs: over the pane's shared connection (in the
/// remote shell's directory), or locally in its directory.
enum ExecTarget {
    Remote(crate::files::remote::Remote, Option<String>),
    Local(Option<std::path::PathBuf>),
}

fn exec_target(pane: &Pane) -> Option<ExecTarget> {
    match (&pane.connection, &pane.detected_ssh) {
        (Some(connection), _) if connection.managed => Some(ExecTarget::Remote(crate::files::remote::Remote::new(connection), pane.remote_cwd.clone())),
        (Some(_), _) | (None, Some(_)) => None,
        (None, None) => Some(ExecTarget::Local(pane.working_dir())),
    }
}

/// Starts a separate command; its output collects in `output`, and its end
/// arrives as `ClaudeEvent::ExecDone`.
fn spawn_exec(target: ExecTarget, command: &str, output: Arc<Mutex<String>>, job: u64, proxy: EventLoopProxy<UserEvent>) -> Result<Arc<Mutex<std::process::Child>>, String> {
    use std::process::{Command, Stdio};

    let mut process = match target {
        ExecTarget::Remote(remote, cwd) => {
            let cd = cwd.map(|dir| format!("cd {} 2>/dev/null; ", crate::files::remote::quote(&dir))).unwrap_or_default();
            remote.command(&format!("{cd}exec 2>&1; {command}"))
        }
        ExecTarget::Local(cwd) => {
            let mut c = Command::new("sh");
            c.args(["-c", &format!("exec 2>&1; {command}")]);
            if let Some(dir) = cwd.filter(|d| d.is_dir()) {
                c.current_dir(dir);
            }
            c
        }
    };
    process.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = process.spawn().map_err(|e| format!("Couldn't start it: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("No output pipe.")?;
    let child = Arc::new(Mutex::new(child));
    let waiter = child.clone();
    std::thread::Builder::new()
        .name("claude-exec".into())
        .spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = stdout.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut out) = output.lock() {
                    out.push_str(&String::from_utf8_lossy(&chunk[..n]));
                    // Keep the end; Claude only ever gets that.
                    if out.len() > 4 * MAX_OUTPUT {
                        let cut = out.len() - 2 * MAX_OUTPUT;
                        let cut = (cut..out.len()).find(|&i| out.is_char_boundary(i)).unwrap_or(cut);
                        out.drain(..cut);
                    }
                }
            }
            // Polled, so the lock is free for a cancel in between.
            let result = loop {
                let polled = waiter.lock().map_err(|_| "lost the process".to_string()).and_then(|mut c| c.try_wait().map_err(|e| e.to_string()));
                match polled {
                    Ok(Some(status)) => break Ok(status.code().unwrap_or(-1)),
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Err(why) => break Err(why),
                }
            };
            let _ = proxy.send_event(UserEvent::Claude(ClaudeEvent::ExecDone { job, result }));
        })
        .map_err(|e| format!("Couldn't start it: {e}"))?;
    Ok(child)
}

/// The end of a long text, saying how much was left out.
fn tail(text: &str) -> String {
    if text.len() <= MAX_OUTPUT {
        return text.to_string();
    }
    let mut cut = text.len() - MAX_OUTPUT;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    let dropped = text[..cut].lines().count();
    format!("[{dropped} earlier lines left out]\n{}", &text[cut..])
}

/// Claude's keys only act while their feature is on; otherwise the key goes
/// to the program as if unbound.
pub(super) fn key_enabled(c: &crate::config::ClaudeConfig, action: crate::input::Action) -> bool {
    use crate::input::Action;
    match action {
        Action::ClaudeAgents | Action::ClaudeNextWaiting => c.overview,
        Action::AskClaude => c.ask,
        Action::ShareWithClaude => c.panes,
        _ => true,
    }
}

impl App {
    /// "Ask Claude" from the keyboard or the palette: the selection, else the
    /// current command and its output, as Markdown.
    pub(super) fn ask_claude_current(&mut self) {
        let selection = self.window.as_ref().and_then(|w| w.active_pane().selected_text()).filter(|t| !t.trim().is_empty());
        if let Some(text) = selection {
            return self.ask_claude(format!("```\n{}\n```\n", text.trim_end()));
        }
        let Some((pane, block)) = self.current_block() else {
            if let Some(w) = &mut self.window {
                w.show_toast("Nothing to ask about: select some text, or run a command first");
            }
            return;
        };
        self.ask_claude_block(pane, &block);
    }

    /// "Ask Claude" for one command block.
    pub(super) fn ask_claude_block(&mut self, pane: PaneId, block: &crate::blocks::Block) {
        let Some(w) = &self.window else { return };
        let Some(p) = w.tabs.iter().find_map(|t| t.pane(pane)) else { return };
        let output = block.output.map(|(start, end)| p.lock().text_between((start, 0), (end, p.grid.0.saturating_sub(1)))).unwrap_or_default();
        let mut text = crate::blocks::markdown(block.command.as_deref(), &tail(output.trim_end()));
        match block.status {
            Some(0) | None => {}
            Some(code) => text.push_str(&format!("It exited with code {code}.\n")),
        }
        if let Some(host) = p.connection.as_ref().map(|c| c.alias.clone()).or(p.detected_ssh.as_ref().map(|d| d.host().to_string())) {
            text.push_str(&format!("This ran on the server {host}.\n"));
        }
        self.ask_claude(text);
    }
}
