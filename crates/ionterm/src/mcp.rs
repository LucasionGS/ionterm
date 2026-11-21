//! `ionterm mcp`: the MCP server the Claude Code plugin starts. It gives
//! Claude tools for the terminal panes you share with it (list them, read
//! their output, run commands in them), each one a request to the window over
//! the remote control socket (`app/claude.rs` answers them).
//!
//! The protocol is JSON-RPC 2.0, one message per line on stdin and stdout.
//! Nothing else may be printed to stdout.

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2025-06-18";

/// How long `run` waits for a command before handing back a job to poll.
const DEFAULT_WAIT: u64 = 30;
/// The longest any one tool call waits, under Claude Code's tool timeouts.
const MAX_WAIT: u64 = 120;
const POLL: Duration = Duration::from_millis(200);

const INSTRUCTIONS: &str = "\
These tools reach the terminal panes the user shares with you in ionterm, often SSH sessions to their servers. \
You only see panes the user chose to share (\"Share with Claude\" in ionterm). \
Commands run as the user on that machine: read-only ones run at once, anything else waits until the user allows it in ionterm. \
Prefer mode \"auto\": it types into the visible shell when it's idle at its prompt (so its directory, environment and sudo session apply), \
and runs the command separately over the same connection when the shell is busy. Use \"exec\" to run several things at once. \
Don't run interactive programs (editors, pagers, top); pass flags like --no-pager or -n 50 instead.";

pub fn main() -> ExitCode {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(&message),
            Err(error) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": error.to_string()}})),
        };
        if let Some(reply) = reply
            && writeln!(stdout, "{reply}").and_then(|_| stdout.flush()).is_err()
        {
            break;
        }
    }
    ExitCode::SUCCESS
}

/// The answer to one message; `None` for notifications.
fn handle(message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message["method"].as_str().unwrap_or_default();
    let result = match method {
        "initialize" => {
            let version = message["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "ionterm", "version": crate::VERSION},
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => Ok(call(message["params"]["name"].as_str().unwrap_or_default(), &message["params"]["arguments"])),
        _ => Err(json!({"code": -32601, "message": format!("Unknown method {method}")})),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
    })
}

fn tools() -> Value {
    let pane = json!({"type": "integer", "description": "The pane's id, from list_panes."});
    json!([
        {
            "name": "list_panes",
            "description": "List the terminal panes the user shares with you: id, host (or local), the shell's directory, whether the shell is idle at its prompt, and its last command with its exit code.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true, "title": "List shared panes"},
        },
        {
            "name": "read_pane",
            "description": "Read a shared pane's text: its screen, its last N lines of scrollback, or the output of one of its recent commands (from shell integration).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": pane,
                    "what": {"type": "string", "enum": ["screen", "lines", "command"], "description": "screen (default), the last `lines` lines, or the output of a command."},
                    "lines": {"type": "integer", "description": "With what=lines: how many. Default 200."},
                    "command": {"type": "integer", "description": "With what=command: 0 for the last command (default), 1 for the one before, and so on."},
                },
                "required": ["pane"],
            },
            "annotations": {"readOnlyHint": true, "title": "Read a pane"},
        },
        {
            "name": "run",
            "description": "Run a shell command in a shared pane and return its output and exit code. Commands that aren't read-only wait for the user to allow them. If it's still running after `wait_seconds`, you get a job id: call `job` to keep waiting, or `cancel` to stop it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pane": pane,
                    "command": {"type": "string", "description": "One shell command line."},
                    "mode": {"type": "string", "enum": ["auto", "shell", "exec"], "description": "auto (default): typed into the visible shell when it's idle, else exec. shell: typed into the visible shell (its directory, environment and sudo session apply). exec: a separate non-interactive shell over the same connection, beside the user's, in the shell's directory."},
                    "wait_seconds": {"type": "integer", "description": "How long to wait for it to finish before returning a job id. Default 30, at most 120."},
                },
                "required": ["pane", "command"],
            },
            "annotations": {"destructiveHint": true, "title": "Run a command"},
        },
        {
            "name": "job",
            "description": "Wait for a command started by `run` and get its output so far, or all of it with the exit code once it's done.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "job": {"type": "integer"},
                    "wait_seconds": {"type": "integer", "description": "How long to wait for it to finish. Default 30, at most 120."},
                },
                "required": ["job"],
            },
            "annotations": {"readOnlyHint": true, "title": "Check a command"},
        },
        {
            "name": "cancel",
            "description": "Stop a command started by `run`: ctrl+c in the shell, or end the separate one.",
            "inputSchema": {"type": "object", "properties": {"job": {"type": "integer"}}, "required": ["job"]},
            "annotations": {"title": "Stop a command"},
        },
    ])
}

/// Runs a tool: text content, flagged as an error when it failed.
fn call(name: &str, args: &Value) -> Value {
    let outcome = match name {
        "list_panes" => request(&["claude-panes".into()]),
        "read_pane" => read_pane(args),
        "run" => run(args),
        "job" => match args["job"].as_u64() {
            Some(job) => wait_for(job, wait(args)),
            None => Err("Missing job.".into()),
        },
        "cancel" => match args["job"].as_u64() {
            Some(job) => request(&["claude-cancel".into(), job.to_string()]),
            None => Err("Missing job.".into()),
        },
        other => Err(format!("No tool called {other}.")),
    };
    match outcome {
        Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
        Err(text) => json!({"content": [{"type": "text", "text": text}], "isError": true}),
    }
}

fn read_pane(args: &Value) -> Result<String, String> {
    let pane = args["pane"].as_u64().ok_or("Missing pane.")?;
    let mut command = vec!["claude-read".to_string(), pane.to_string(), args["what"].as_str().unwrap_or("screen").to_string()];
    match args["what"].as_str() {
        Some("lines") => command.push(args["lines"].as_u64().unwrap_or(200).to_string()),
        Some("command") => command.push(args["command"].as_u64().unwrap_or(0).to_string()),
        _ => {}
    }
    request(&command)
}

fn run(args: &Value) -> Result<String, String> {
    let pane = args["pane"].as_u64().ok_or("Missing pane.")?;
    let command = args["command"].as_str().filter(|c| !c.trim().is_empty()).ok_or("Missing command.")?;
    let mode = args["mode"].as_str().unwrap_or("auto");
    let answer = request(&["claude-run".into(), pane.to_string(), mode.into(), command.into()])?;
    let job: Value = serde_json::from_str(&answer).map_err(|_| answer.clone())?;
    let job = job["job"].as_u64().ok_or(answer)?;
    wait_for(job, wait(args))
}

fn wait(args: &Value) -> Duration {
    Duration::from_secs(args["wait_seconds"].as_u64().unwrap_or(DEFAULT_WAIT).min(MAX_WAIT))
}

/// Polls a job until it's over or `wait` runs out, then describes it.
fn wait_for(job: u64, wait: Duration) -> Result<String, String> {
    let deadline = Instant::now() + wait;
    loop {
        let answer = request(&["claude-job".into(), job.to_string()])?;
        let state: Value = serde_json::from_str(&answer).map_err(|_| answer.clone())?;
        let status = state["state"].as_str().unwrap_or_default();
        let over = matches!(status, "done" | "denied" | "refused" | "cancelled" | "failed");
        if over || Instant::now() >= deadline {
            return describe(job, &state);
        }
        std::thread::sleep(POLL);
    }
}

/// A job's state as text for Claude.
fn describe(job: u64, state: &Value) -> Result<String, String> {
    let output = state["output"].as_str().unwrap_or_default();
    let how = state["mode"].as_str().map(|m| format!(" ({m})")).unwrap_or_default();
    match state["state"].as_str().unwrap_or_default() {
        "done" => {
            let code = state["exit"].as_i64().map_or("unknown".to_string(), |c| c.to_string());
            Ok(format!("Exit code {code}{how}\n{output}"))
        }
        "approval" => Ok(format!("Job {job} is waiting for the user to allow it in ionterm. Call job with job={job} to keep waiting.")),
        "running" => Ok(format!("Job {job} is still running{how}. Call job with job={job} to keep waiting, or cancel to stop it. Output so far:\n{output}")),
        "denied" => Err("The user didn't allow this command.".into()),
        "cancelled" => Ok(format!("Cancelled{how}. Output before that:\n{output}")),
        _ => Err(state["error"].as_str().unwrap_or("The command couldn't run.").to_string()),
    }
}

/// One request to the window this Claude runs in.
fn request(args: &[String]) -> Result<String, String> {
    if std::env::var_os("IONTERM_SOCKET").is_none() {
        return Err("This Claude Code session isn't running inside ionterm, so there are no terminal panes to use.".into());
    }
    match crate::remote::send(args) {
        Some(Ok((0, text))) => Ok(text),
        Some(Ok((_, text))) => Err(text.trim().to_string()),
        Some(Err(error)) => Err(format!("Lost the connection to ionterm: {error}")),
        None => Err("ionterm isn't answering. Is the window still open?".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_initialize_and_lists_tools() {
        let reply = handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}})).unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], "ionterm");
        assert!(handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        let reply = handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
        let names: Vec<&str> = reply["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["list_panes", "read_pane", "run", "job", "cancel"]);
        let reply = handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})).unwrap();
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[test]
    fn describes_finished_jobs() {
        let done = json!({"state": "done", "exit": 2, "mode": "exec", "output": "no such file"});
        assert_eq!(describe(4, &done).unwrap(), "Exit code 2 (exec)\nno such file");
        assert!(describe(4, &json!({"state": "denied"})).is_err());
    }
}
