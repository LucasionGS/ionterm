//! Claude Code support without a window: the bundled plugin and its install,
//! the hook client (`ionterm claude-hook`), the events it sends, each pane's
//! agent state, and which commands from Claude count as read-only.
//!
//! The plugin's hooks run `$IONTERM_BIN claude-hook` with the hook's JSON on
//! stdin. That client keeps only the short fields and sends them to the
//! window as `ionterm @ claude-event key=value…`; the app applies them to the
//! pane's `Agent` (`app/claude.rs`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

/// The plugin's id for `claude plugin`: plugin `ionterm` in marketplace `ionterm`.
pub const PLUGIN_ID: &str = "ionterm@ionterm";

/// The plugin's files, written to `plugin_dir()` before installing.
const PLUGIN_FILES: &[(&str, &str)] = &[
    (".claude-plugin/plugin.json", include_str!("../../../extras/claude-plugin/.claude-plugin/plugin.json")),
    (".claude-plugin/marketplace.json", include_str!("../../../extras/claude-plugin/.claude-plugin/marketplace.json")),
    ("hooks/hooks.json", include_str!("../../../extras/claude-plugin/hooks/hooks.json")),
    (".mcp.json", include_str!("../../../extras/claude-plugin/.mcp.json")),
];

/// Longest prompt, message or tool detail kept, in characters.
const MAX_FIELD: usize = 300;
const MAX_DETAIL: usize = 120;

//#region Plugin

/// Whether `claude` is installed (on `PATH`). Looked up once.
pub fn available() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| claude_program().is_some())
}

fn claude_program() -> Option<PathBuf> {
    let name = if cfg!(windows) { "claude.exe" } else { "claude" };
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(name)).find(|path| path.is_file())
}

/// Where ionterm keeps the plugin it installs from.
pub fn plugin_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|dir| dir.join("ionterm").join("claude-plugin"))
}

/// The bundled plugin's version, from its `plugin.json`.
pub fn plugin_version() -> String {
    let manifest: Value = serde_json::from_str(PLUGIN_FILES[0].1).unwrap_or_default();
    manifest["version"].as_str().unwrap_or_default().to_string()
}

/// The plugin as Claude Code knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginState {
    Missing,
    /// Installed: its version, and whether it's enabled.
    Installed { version: String, enabled: bool },
}

/// Asks `claude plugin list --json`. Slow (it starts Claude Code), so call
/// it off the event loop.
pub fn plugin_state() -> Result<PluginState, String> {
    let output = std::process::Command::new("claude")
        .args(["plugin", "list", "--json"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("Couldn't run claude: {e}"))?;
    if !output.status.success() {
        return Err(format!("claude plugin list failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(parse_plugin_list(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_plugin_list(json: &str) -> PluginState {
    let list: Value = serde_json::from_str(json).unwrap_or_default();
    let found = list.as_array().into_iter().flatten().find(|p| p["id"].as_str() == Some(PLUGIN_ID));
    match found {
        Some(plugin) => PluginState::Installed {
            version: plugin["version"].as_str().unwrap_or_default().to_string(),
            enabled: plugin["enabled"].as_bool().unwrap_or(true),
        },
        None => PluginState::Missing,
    }
}

/// Makes sure the bundled plugin is installed, enabled and current: writes
/// its files, then adds the marketplace and installs, updates or enables it
/// as needed. Returns what it did, for a toast. Slow; call it off the event loop.
pub fn ensure_plugin() -> Result<Option<String>, String> {
    let dir = plugin_dir().ok_or("There's no data folder to keep the plugin in.")?;
    write_plugin(&dir).map_err(|e| format!("Couldn't write the plugin to {}: {e}", dir.display()))?;
    let wanted = plugin_version();
    match plugin_state()? {
        PluginState::Installed { version, enabled } if version == wanted => {
            if enabled {
                return Ok(None);
            }
            run_claude(&["plugin", "enable", PLUGIN_ID])?;
            Ok(Some("Enabled the ionterm plugin for Claude Code.".into()))
        }
        PluginState::Installed { .. } => {
            run_claude(&["plugin", "marketplace", "update", "ionterm"])?;
            run_claude(&["plugin", "update", PLUGIN_ID])?;
            run_claude(&["plugin", "enable", PLUGIN_ID]).ok();
            Ok(Some(format!("Updated the ionterm plugin for Claude Code to {wanted}. Restart running Claude sessions to use it.")))
        }
        PluginState::Missing => {
            let path = dir.display().to_string();
            // Already added (an earlier install that was removed) is fine.
            let _ = run_claude(&["plugin", "marketplace", "add", &path, "--scope", "user"]);
            run_claude(&["plugin", "install", PLUGIN_ID, "--scope", "user"])?;
            Ok(Some("Installed the ionterm plugin for Claude Code. Sessions started from now on report to ionterm.".into()))
        }
    }
}

/// Uninstalls the plugin and forgets its marketplace.
pub fn remove_plugin() -> Result<(), String> {
    run_claude(&["plugin", "uninstall", PLUGIN_ID, "--scope", "user", "-y"])?;
    let _ = run_claude(&["plugin", "marketplace", "remove", "ionterm"]);
    Ok(())
}

fn write_plugin(dir: &Path) -> std::io::Result<()> {
    for (name, text) in PLUGIN_FILES {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::read_to_string(&path).ok().as_deref() != Some(*text) {
            std::fs::write(&path, text)?;
        }
    }
    Ok(())
}

fn run_claude(args: &[&str]) -> Result<(), String> {
    let output = std::process::Command::new("claude")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("Couldn't run claude: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&output.stderr);
    let why = if why.trim().is_empty() { String::from_utf8_lossy(&output.stdout) } else { why };
    Err(format!("claude {} failed: {}", args.join(" "), why.trim()))
}

//#endregion

//#region Hook client

/// `ionterm claude-hook`: the plugin's hooks run it with the hook's JSON on
/// stdin. It never fails the hook: Claude Code must not notice ionterm.
pub fn hook_main() -> ExitCode {
    use std::io::Read;

    // Hooks run asynchronously and may finish out of order; the window
    // keeps the newest by when each one started.
    let ts = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let mut input = String::new();
    if std::io::stdin().take(32 << 20).read_to_string(&mut input).is_err() {
        return ExitCode::SUCCESS;
    }
    let Ok(hook) = serde_json::from_str::<Value>(&input) else { return ExitCode::SUCCESS };
    let args = hook_args(&hook, ts);
    if !args.is_empty() {
        let _ = crate::remote::send_within(&args, Duration::from_secs(3));
    }
    ExitCode::SUCCESS
}

/// The `claude-event` command for one hook's JSON: only short fields, so a
/// huge tool response never travels.
fn hook_args(hook: &Value, ts: u128) -> Vec<String> {
    let Some(event) = hook["hook_event_name"].as_str() else { return Vec::new() };
    let mut args = vec!["claude-event".to_string(), format!("event={event}"), format!("ts={ts}")];
    let mut put = |key: &str, value: Option<&str>, max: usize| {
        if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
            args.push(format!("{key}={}", clip(value, max)));
        }
    };
    put("session", hook["session_id"].as_str(), MAX_FIELD);
    put("cwd", hook["cwd"].as_str(), 4096);
    let tool = hook["tool_name"].as_str();
    match event {
        "SessionStart" => put("source", hook["source"].as_str(), 40),
        "SessionEnd" => put("reason", hook["reason"].as_str(), 40),
        "UserPromptSubmit" => {
            put("prompt", hook["prompt"].as_str(), MAX_FIELD);
            put("title", hook["session_title"].as_str(), MAX_FIELD);
        }
        "Notification" => {
            put("kind", hook["notification_type"].as_str(), 60);
            put("message", hook["message"].as_str(), MAX_FIELD);
        }
        "Stop" => put("last", hook["last_assistant_message"].as_str(), MAX_FIELD),
        "StopFailure" => put("error", hook["error"].as_str().or(hook["error_details"].as_str()), MAX_FIELD),
        _ => {}
    }
    if let Some(tool) = tool {
        let input = &hook["tool_input"];
        put("tool", Some(tool), 100);
        put("detail", Some(&tool_detail(tool, input)), MAX_DETAIL);
        if event == "PostToolUse" {
            for todo in todo_fields(tool, input, &hook["tool_response"]) {
                args.push(todo);
            }
        }
    }
    args
}

/// A few words on what a tool call does: the command, the file, the pattern.
fn tool_detail(tool: &str, input: &Value) -> String {
    let field = |key: &str| input[key].as_str().unwrap_or_default().to_string();
    let detail = match tool {
        "Bash" | "PowerShell" => field("command").lines().next().unwrap_or_default().to_string(),
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            let path = input["file_path"].as_str().or(input["notebook_path"].as_str()).unwrap_or_default();
            Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        }
        "Grep" | "Glob" => field("pattern"),
        "WebFetch" => field("url"),
        "WebSearch" => field("query"),
        "Agent" | "Task" => field("description"),
        "Skill" => field("skill"),
        _ => String::new(),
    };
    detail.trim().to_string()
}

/// The todo list's changes: `todos=done/total` and `todo=…` for the legacy
/// `TodoWrite` (the whole list each time), or `task=id\tstatus\ttext` for
/// the task tools (one task at a time).
fn todo_fields(tool: &str, input: &Value, response: &Value) -> Vec<String> {
    match tool {
        "TodoWrite" => {
            let todos = input["todos"].as_array().cloned().unwrap_or_default();
            let done = todos.iter().filter(|t| t["status"].as_str() == Some("completed")).count();
            let active = todos
                .iter()
                .find(|t| t["status"].as_str() == Some("in_progress"))
                .and_then(|t| t["activeForm"].as_str().or(t["content"].as_str()));
            let mut out = vec![format!("todos={done}/{}", todos.len())];
            if let Some(active) = active {
                out.push(format!("todo={}", clip(active, MAX_DETAIL)));
            }
            out
        }
        "TaskCreate" => {
            let id = response["task"]["id"]
                .as_str()
                .map(str::to_string)
                .or(response["task"]["id"].as_u64().map(|n| n.to_string()))
                .or(response["id"].as_str().map(str::to_string))
                .or_else(|| response.as_str().and_then(task_number))
                .unwrap_or_default();
            let text = input["subject"].as_str().unwrap_or_default();
            vec![format!("task={id}\tpending\t{}", clip(text, MAX_DETAIL))]
        }
        "TaskUpdate" => {
            let id = ["taskId", "id", "task_id"]
                .iter()
                .find_map(|key| input[key].as_str().map(str::to_string).or(input[key].as_u64().map(|n| n.to_string())))
                .unwrap_or_default();
            let status = input["status"].as_str().unwrap_or_default();
            let text = input["activeForm"].as_str().or(input["active_form"].as_str()).or(input["subject"].as_str()).unwrap_or_default();
            vec![format!("task={id}\t{status}\t{}", clip(text, MAX_DETAIL))]
        }
        _ => Vec::new(),
    }
}

/// "Task #3 created" → "3".
fn task_number(text: &str) -> Option<String> {
    let after = text.split('#').nth(1)?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then_some(digits)
}

/// At most `max` characters, on one line, with an ellipsis when cut.
pub fn clip(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

//#endregion

//#region Events and agent state

/// One hook's report, as the window receives it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Event {
    pub name: String,
    /// When the hook started, in nanoseconds since 1970.
    pub ts: u128,
    fields: Vec<(String, String)>,
}

impl Event {
    /// From `claude-event`'s `key=value` words.
    pub fn parse(words: &[String]) -> Result<Self, String> {
        let mut event = Event::default();
        for word in words {
            let Some((key, value)) = word.split_once('=') else {
                return Err(format!("Expected key=value, got {word:?}."));
            };
            match key {
                "event" => event.name = value.to_string(),
                "ts" => event.ts = value.parse().unwrap_or(0),
                _ => event.fields.push((key.to_string(), value.to_string())),
            }
        }
        if event.name.is_empty() {
            return Err("Missing event=NAME.".into());
        }
        Ok(event)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    fn all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.fields.iter().filter(move |(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// What Claude is up to in a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Waiting at its prompt for you to type.
    Idle,
    /// Thinking or running tools.
    Working,
    /// Stopped until you answer: a permission prompt, a question, a plan.
    NeedsYou,
}

#[derive(Clone, Debug, PartialEq)]
struct Task {
    id: String,
    status: String,
    text: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
enum Todos {
    #[default]
    None,
    /// `TodoWrite`'s counts.
    List { done: usize, total: usize, active: Option<String> },
    /// The task tools' tasks, in creation order.
    Tasks(Vec<Task>),
}

/// The Claude Code session running in a pane, as its hooks report it.
#[derive(Clone, Debug)]
pub struct Agent {
    pub session: String,
    /// When the pane's session (this one, or one before it) was first seen.
    pub created: Instant,
    pub cwd: Option<PathBuf>,
    pub status: Status,
    /// When `status` last changed.
    pub since: Instant,
    /// When the current (or last) turn started.
    pub turn_started: Option<Instant>,
    /// Your last prompt.
    pub prompt: Option<String>,
    /// The session's title, when Claude Code has one.
    pub title: Option<String>,
    /// What it's doing now: "Bash: cargo test".
    pub doing: Option<String>,
    /// Why it needs you, while it does.
    pub attention: Option<String>,
    /// The end of its last answer.
    pub last_message: Option<String>,
    /// The last turn ended on an error (StopFailure).
    pub failed: bool,
    todos: Todos,
    last_ts: u128,
}

/// What an event changed, for marks and notifications.
#[derive(Clone, Debug, PartialEq)]
pub enum Transition {
    /// Too old (an async hook that finished late), or nothing visible.
    None,
    Changed,
    /// It started waiting for you.
    NeedsYou,
    /// A turn ended; how long it took.
    Finished(Duration),
    /// The session ended: the pane has no agent any more.
    Ended,
}

impl Agent {
    fn new(session: &str, now: Instant) -> Self {
        Self {
            session: session.to_string(),
            created: now,
            cwd: None,
            status: Status::Idle,
            since: now,
            turn_started: None,
            prompt: None,
            title: None,
            doing: None,
            attention: None,
            last_message: None,
            failed: false,
            todos: Todos::None,
            last_ts: 0,
        }
    }

    /// Applies one event to the pane's agent, creating or dropping it.
    pub fn apply(slot: &mut Option<Agent>, event: &Event, now: Instant) -> Transition {
        let session = event.get("session").unwrap_or_default();
        if event.name == "SessionEnd" {
            // `/clear` ends one session and starts another straight away;
            // don't let the late end of the old one drop the new one.
            if slot.as_ref().is_some_and(|a| a.session == session || session.is_empty()) {
                *slot = None;
                return Transition::Ended;
            }
            return Transition::None;
        }
        let fresh = event.name == "SessionStart" || slot.as_ref().is_none_or(|a| a.session != session);
        if fresh {
            // An old session's late event after a new one started.
            if slot.as_ref().is_some_and(|a| a.last_ts > event.ts && a.session != session) {
                return Transition::None;
            }
            // `/clear` starts a new session in the same Claude.
            let created = slot.as_ref().map_or(now, |a| a.created);
            *slot = Some(Agent { created, ..Agent::new(session, now) });
        }
        let Some(agent) = slot.as_mut() else { return Transition::None };
        if event.ts != 0 && event.ts < agent.last_ts {
            // Todo changes still count; their order matters less than losing them.
            agent.apply_todos(event);
            return Transition::None;
        }
        agent.last_ts = event.ts;
        if let Some(cwd) = event.get("cwd") {
            agent.cwd = Some(PathBuf::from(cwd));
        }
        let before = agent.status;
        let doing = || {
            let tool = event.get("tool")?;
            Some(match event.get("detail") {
                Some(detail) => format!("{}: {detail}", short_tool(tool)),
                None => short_tool(tool).to_string(),
            })
        };
        let mut transition = Transition::Changed;
        match event.name.as_str() {
            "SessionStart" => {}
            "UserPromptSubmit" => {
                agent.set(Status::Working, now);
                agent.turn_started = Some(now);
                agent.prompt = event.get("prompt").map(str::to_string);
                if let Some(title) = event.get("title") {
                    agent.title = Some(title.to_string());
                }
                agent.failed = false;
                agent.doing = None;
            }
            "PreToolUse" => {
                agent.doing = doing();
                match event.get("tool") {
                    Some("AskUserQuestion") => agent.wait("Claude has a question for you", now),
                    Some("ExitPlanMode") => agent.wait("Claude's plan is ready for review", now),
                    _ => agent.set(Status::Working, now),
                }
            }
            "PermissionRequest" => {
                let what = doing().unwrap_or_else(|| "a tool".into());
                agent.wait(&format!("Allow {what}?"), now);
            }
            "PostToolUse" | "PostToolUseFailure" => {
                agent.apply_todos(event);
                agent.set(Status::Working, now);
            }
            "PreCompact" => {
                agent.doing = Some("Compacting the conversation".into());
            }
            "Notification" => match event.get("kind") {
                Some("permission_prompt" | "elicitation_dialog" | "elicitation_url_dialog" | "agent_needs_input") => {
                    let message = event.get("message").unwrap_or("Claude needs your input");
                    // A permission request already said what it wants.
                    if agent.status != Status::NeedsYou {
                        agent.wait(message, now);
                    }
                }
                _ => transition = Transition::None,
            },
            "Stop" | "StopFailure" => {
                agent.set(Status::Idle, now);
                agent.doing = None;
                agent.failed = event.name == "StopFailure";
                agent.last_message = event.get("last").or(event.get("error")).map(str::to_string);
            }
            _ => transition = Transition::None,
        }
        if agent.status == Status::NeedsYou && before != Status::NeedsYou {
            return Transition::NeedsYou;
        }
        if agent.status == Status::Idle && before != Status::Idle {
            let took = agent.turn_started.map(|at| now.duration_since(at)).unwrap_or_default();
            return Transition::Finished(took);
        }
        transition
    }

    fn set(&mut self, status: Status, now: Instant) {
        if self.status != status {
            self.status = status;
            self.since = now;
        }
        if status != Status::NeedsYou {
            self.attention = None;
        }
    }

    fn wait(&mut self, why: &str, now: Instant) {
        self.set(Status::NeedsYou, now);
        self.attention = Some(why.to_string());
    }

    fn apply_todos(&mut self, event: &Event) {
        if let Some((done, total)) = event.get("todos").and_then(|t| t.split_once('/')) {
            self.todos = Todos::List {
                done: done.parse().unwrap_or(0),
                total: total.parse().unwrap_or(0),
                active: event.get("todo").map(str::to_string),
            };
        }
        for task in event.all("task") {
            let mut parts = task.splitn(3, '\t');
            let (id, status, text) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
            if !matches!(self.todos, Todos::Tasks(_)) {
                self.todos = Todos::Tasks(Vec::new());
            }
            let Todos::Tasks(tasks) = &mut self.todos else { continue };
            match tasks.iter_mut().find(|t| !id.is_empty() && t.id == id) {
                Some(_) if status == "deleted" => tasks.retain(|t| t.id != id),
                Some(existing) => {
                    if !status.is_empty() {
                        existing.status = status.to_string();
                    }
                    if !text.is_empty() {
                        existing.text = text.to_string();
                    }
                }
                None if status != "deleted" => tasks.push(Task {
                    id: id.to_string(),
                    status: if status.is_empty() { "pending".into() } else { status.to_string() },
                    text: text.to_string(),
                }),
                None => {}
            }
        }
    }

    /// The todo list: done, total and the one in progress. `None` without one.
    pub fn todos(&self) -> Option<(usize, usize, Option<&str>)> {
        match &self.todos {
            Todos::None => None,
            Todos::List { total: 0, .. } => None,
            Todos::List { done, total, active } => Some((*done, *total, active.as_deref())),
            Todos::Tasks(tasks) if tasks.is_empty() => None,
            Todos::Tasks(tasks) => {
                let done = tasks.iter().filter(|t| t.status == "completed").count();
                let active = tasks.iter().find(|t| t.status == "in_progress").map(|t| t.text.as_str());
                Some((done, tasks.len(), active))
            }
        }
    }

    /// The tab's progress bar: the share of todos done, while some aren't.
    pub fn todo_fraction(&self) -> Option<f32> {
        let (done, total, _) = self.todos()?;
        (done < total && self.status != Status::Idle).then(|| done as f32 / total as f32)
    }

    /// The project: the last part of its directory.
    pub fn project(&self) -> String {
        self.cwd
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Claude".into())
    }

    /// One line on its state, for the popup and notifications.
    pub fn describe(&self, now: Instant) -> String {
        let ago = crate::notifications::duration(now.duration_since(self.since));
        match self.status {
            Status::NeedsYou => self.attention.clone().unwrap_or_else(|| "Needs you".into()),
            Status::Working => {
                let todo = self.todos().map(|(done, total, active)| match active {
                    Some(active) => format!("{active} ({done}/{total})"),
                    None => format!("{done}/{total} todos"),
                });
                let what = todo.or(self.doing.clone()).unwrap_or_else(|| "Thinking".into());
                format!("{what} · {ago}")
            }
            Status::Idle if self.failed => format!("Stopped on an error {ago} ago"),
            Status::Idle if self.turn_started.is_some() => format!("Finished {ago} ago"),
            Status::Idle => "Waiting for a prompt".into(),
        }
    }
}

/// `mcp__ionterm__run` → `run`, so tool names read well in a tab.
fn short_tool(tool: &str) -> &str {
    tool.strip_prefix("mcp__").and_then(|t| t.rsplit("__").next()).unwrap_or(tool)
}

/// A spinner frame for working agents, changing every `SPINNER_FRAME`.
pub const SPINNER_FRAME: Duration = Duration::from_millis(100);

pub fn spinner(now: Instant) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    static START: OnceLock<Instant> = OnceLock::new();
    let start = *START.get_or_init(|| now);
    let step = now.saturating_duration_since(start).as_millis() / SPINNER_FRAME.as_millis();
    FRAMES[(step % FRAMES.len() as u128) as usize]
}

//#endregion

//#region Read-only commands

/// Programs whose every use only reads.
const READ_ONLY: &[&str] = &[
    "ls", "ll", "cat", "head", "tail", "grep", "egrep", "fgrep", "rg", "wc", "stat", "file", "du", "df", "free", "uptime", "uname",
    "hostname", "hostnamectl", "whoami", "id", "groups", "ps", "pgrep", "pwd", "date", "which", "type", "whereis", "printenv", "echo",
    "realpath", "readlink", "basename", "dirname", "tree", "lsblk", "lscpu", "lsusb", "lspci", "lsof", "ss", "netstat", "dig",
    "nslookup", "host", "sensors", "nproc", "md5sum", "sha256sum", "sort", "uniq", "cut", "tr", "column", "jq", "diff", "cmp",
    "journalctl", "last", "w", "who", "getent", "timedatectl", "vmstat", "iostat", "ping", "true", "test", "[", "zcat",
];

/// Commands that read only with these first arguments.
const READ_ONLY_SUBCOMMANDS: &[&str] = &[
    "systemctl status", "systemctl is-active", "systemctl is-enabled", "systemctl is-failed", "systemctl list-units",
    "systemctl list-unit-files", "systemctl list-timers", "systemctl show", "systemctl cat", "docker ps", "docker images",
    "docker logs", "docker inspect", "docker stats", "docker compose ps", "docker compose logs", "docker compose config",
    "docker-compose ps", "docker-compose logs", "podman ps", "podman logs", "podman images", "git status", "git log", "git diff",
    "git show", "git branch", "git remote", "git rev-parse", "git blame", "ip a", "ip addr", "ip address", "ip route", "ip r",
    "ip link", "ip -br", "nginx -t", "nginx -T", "pm2 list", "pm2 ls", "pm2 status", "pm2 logs", "pm2 show", "crontab -l",
    "apt list", "apt show", "apt-cache", "apt policy", "dpkg -l", "dpkg -s", "pacman -Q", "pacman -Ss", "pacman -Si", "free -h",
    "kubectl get", "kubectl describe", "kubectl logs", "ufw status", "certbot certificates", "sed -n", "find",
];

/// Whether a command Claude wants to run only reads, so it can run without
/// asking. Conservative: any write, redirection, command substitution,
/// background job or `sudo` makes it ask. `extra` adds the user's own
/// entries (`[claude] read_only`).
pub fn is_read_only(command: &str, extra: &[String]) -> bool {
    let command = command.trim();
    if command.is_empty() || command.contains(['\n', ';', '`', '>']) || command.contains("$(") || command.contains("<(") {
        return false;
    }
    // A single `&` runs something in the background; `&&` is fine.
    if command.replace("&&", "").contains('&') {
        return false;
    }
    command
        .split("&&")
        .flat_map(|part| part.split("||"))
        .flat_map(|part| part.split('|'))
        .all(|segment| segment_reads_only(segment, extra))
}

fn segment_reads_only(segment: &str, extra: &[String]) -> bool {
    let words: Vec<&str> = segment.split_whitespace().collect();
    let Some(&program) = words.first() else { return false };
    // Variables set in front (`LANG=C ls`) could change what runs.
    if program.contains('=') {
        return false;
    }
    if matches!(program, "find") && words.iter().any(|w| matches!(*w, "-exec" | "-execdir" | "-delete" | "-ok" | "-okdir" | "-fprint" | "-fprintf" | "-fls")) {
        return false;
    }
    // `sed -n` prints; any in-place edit doesn't count.
    if program == "sed" && words.iter().any(|w| w.starts_with("-i") || w.starts_with("--in-place")) {
        return false;
    }
    // `journalctl --vacuum…`, `--rotate` and the like change the journal.
    if program == "journalctl" && words.iter().any(|w| w.starts_with("--vacuum") || *w == "--rotate" || *w == "--flush") {
        return false;
    }
    let matches_prefix = |entry: &str| {
        let entry: Vec<&str> = entry.split_whitespace().collect();
        !entry.is_empty() && words.len() >= entry.len() && words[..entry.len()] == entry[..]
    };
    if READ_ONLY.contains(&program) && program != "find" && program != "sed" {
        return true;
    }
    READ_ONLY_SUBCOMMANDS.iter().any(|entry| matches_prefix(entry)) || extra.iter().any(|entry| matches_prefix(entry))
}

//#endregion

#[cfg(test)]
mod tests {
    use super::*;

    fn event(words: &[&str]) -> Event {
        Event::parse(&words.iter().map(|w| w.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn hook_json_becomes_short_fields() {
        let hook: Value = serde_json::from_str(
            r#"{"hook_event_name":"PreToolUse","session_id":"s1","cwd":"/home/ion/dev/ionterm",
                "tool_name":"Bash","tool_input":{"command":"cargo test --workspace\necho done"},"tool_response":"x"}"#,
        )
        .unwrap();
        let args = hook_args(&hook, 7);
        assert_eq!(
            args,
            ["claude-event", "event=PreToolUse", "ts=7", "session=s1", "cwd=/home/ion/dev/ionterm", "tool=Bash", "detail=cargo test --workspace"]
        );
    }

    #[test]
    fn todos_from_both_tools() {
        let input: Value = serde_json::from_str(
            r#"{"todos":[{"content":"a","status":"completed","activeForm":"Doing a"},{"content":"b","status":"in_progress","activeForm":"Doing b"},{"content":"c","status":"pending"}]}"#,
        )
        .unwrap();
        assert_eq!(todo_fields("TodoWrite", &input, &Value::Null), ["todos=1/3", "todo=Doing b"]);
        let created: Value = serde_json::from_str(r#"{"task":{"id":"4","subject":"Fix it"}}"#).unwrap();
        let input: Value = serde_json::from_str(r#"{"subject":"Fix it","activeForm":"Fixing it"}"#).unwrap();
        assert_eq!(todo_fields("TaskCreate", &input, &created), ["task=4\tpending\tFix it"]);
        let input: Value = serde_json::from_str(r#"{"taskId":"4","status":"in_progress","activeForm":"Fixing it"}"#).unwrap();
        assert_eq!(todo_fields("TaskUpdate", &input, &Value::Null), ["task=4\tin_progress\tFixing it"]);
    }

    #[test]
    fn a_turn_goes_working_then_needs_you_then_idle() {
        let now = Instant::now();
        let mut slot = None;
        assert_eq!(Agent::apply(&mut slot, &event(&["event=SessionStart", "ts=1", "session=a", "cwd=/x/proj"]), now), Transition::Changed);
        assert_eq!(slot.as_ref().unwrap().status, Status::Idle);
        Agent::apply(&mut slot, &event(&["event=UserPromptSubmit", "ts=2", "session=a", "prompt=fix it"]), now);
        assert_eq!(slot.as_ref().unwrap().status, Status::Working);
        let t = Agent::apply(&mut slot, &event(&["event=PermissionRequest", "ts=3", "session=a", "tool=Bash", "detail=rm -rf target"]), now);
        assert_eq!(t, Transition::NeedsYou);
        assert_eq!(slot.as_ref().unwrap().attention.as_deref(), Some("Allow Bash: rm -rf target?"));
        // The permission notification that follows doesn't replace the reason.
        let t = Agent::apply(&mut slot, &event(&["event=Notification", "ts=4", "session=a", "kind=permission_prompt", "message=Claude needs your permission"]), now);
        assert_eq!(t, Transition::Changed);
        assert_eq!(slot.as_ref().unwrap().attention.as_deref(), Some("Allow Bash: rm -rf target?"));
        Agent::apply(&mut slot, &event(&["event=PostToolUse", "ts=5", "session=a", "tool=Bash", "task=1\tin_progress\tFixing"]), now);
        assert_eq!(slot.as_ref().unwrap().status, Status::Working);
        assert_eq!(slot.as_ref().unwrap().todos(), Some((0, 1, Some("Fixing"))));
        assert!(matches!(Agent::apply(&mut slot, &event(&["event=Stop", "ts=6", "session=a", "last=Done."]), now), Transition::Finished(_)));
        assert_eq!(slot.as_ref().unwrap().project(), "proj");
        assert_eq!(Agent::apply(&mut slot, &event(&["event=SessionEnd", "ts=7", "session=a"]), now), Transition::Ended);
        assert!(slot.is_none());
    }

    #[test]
    fn late_async_hooks_dont_undo_newer_state() {
        let now = Instant::now();
        let mut slot = None;
        Agent::apply(&mut slot, &event(&["event=UserPromptSubmit", "ts=10", "session=a"]), now);
        Agent::apply(&mut slot, &event(&["event=Stop", "ts=30", "session=a"]), now);
        // A PostToolUse that started before the Stop but arrived after it.
        assert_eq!(Agent::apply(&mut slot, &event(&["event=PostToolUse", "ts=20", "session=a", "todos=2/2"]), now), Transition::None);
        assert_eq!(slot.as_ref().unwrap().status, Status::Idle);
        // /clear: the old session's end arrives after the new one started.
        Agent::apply(&mut slot, &event(&["event=SessionStart", "ts=40", "session=b"]), now);
        assert_eq!(Agent::apply(&mut slot, &event(&["event=SessionEnd", "ts=35", "session=a"]), now), Transition::None);
        assert_eq!(slot.as_ref().unwrap().session, "b");
    }

    #[test]
    fn read_only_commands() {
        let extra = vec!["mytool show".to_string()];
        for ok in ["ls -la /etc", "tail -n 50 /var/log/syslog", "systemctl status nginx", "docker ps -a", "journalctl -u nginx -n 100 | grep error", "git log --oneline && git status", "mytool show x", "find . -name '*.rs'"] {
            assert!(is_read_only(ok, &extra), "{ok}");
        }
        for ask in ["systemctl restart nginx", "rm -rf /", "ls; rm x", "cat a > b", "echo $(id)", "sudo ls", "find . -delete", "sed -i s/a/b/ f", "LD_PRELOAD=x ls", "sleep 9 &", "docker rm x", "git push", "curl http://x", ""] {
            assert!(!is_read_only(ask, &extra), "{ask}");
        }
    }

    #[test]
    fn finds_the_plugin_in_the_list() {
        let json = r#"[{"id":"other@x","version":"1","enabled":true},{"id":"ionterm@ionterm","version":"0.1.0","enabled":false}]"#;
        assert_eq!(parse_plugin_list(json), PluginState::Installed { version: "0.1.0".into(), enabled: false });
        assert_eq!(parse_plugin_list("[]"), PluginState::Missing);
    }
}
