//! Spawning the shell and pumping its output into the terminal.
//!
//! portable-pty gives one API over Unix PTYs and Windows ConPTY. Each pane
//! gets a reader thread that parses output straight into the shared [`Term`]
//! and nudges the event loop, coalescing nudges so a flood of output costs one
//! redraw per frame rather than one per read. When `sz` or `rz` starts a
//! ZMODEM transfer, the reader hands the bytes to a [`zmodem::Session`]
//! instead, until it's over.

use std::borrow::Cow;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use ionterm_vt::{Processor, Term};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;
use crate::config::TerminalConfig;
use crate::layout::PaneId;
use crate::zmodem;

pub type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Where a PTY's output goes: the pane's terminal, and the event loop to
/// wake when there is something to draw.
pub struct OutputSink {
    pub pane: PaneId,
    pub term: Arc<Mutex<Term>>,
    pub proxy: EventLoopProxy<UserEvent>,
    pub wakeup_pending: Arc<AtomicBool>,
    pub zmodem: Arc<zmodem::Shared>,
}

pub struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: SharedWriter,
    child: Box<dyn Child + Send + Sync>,
}

impl Pty {
    pub fn spawn(
        config: &TerminalConfig,
        argv: Option<&[String]>,
        cwd: Option<&Path>,
        size: PtySize,
        sink: OutputSink,
    ) -> anyhow::Result<Self> {
        let OutputSink {
            pane,
            term,
            proxy,
            wakeup_pending,
            zmodem,
        } = sink;
        let pair = native_pty_system().openpty(size).context("opening a PTY")?;

        // An explicit command (an ssh connection), `ionterm -e`, or the shell.
        let words = match argv.or(Some(config.command.as_slice())).and_then(|a| a.split_first()) {
            Some((program, args)) => Some((program.clone(), args.to_vec())),
            None => shell_words(&config.shell),
        };
        let mut command = match words {
            Some((program, args)) => {
                let mut command = CommandBuilder::new(program);
                command.args(args);
                command
            }
            None => CommandBuilder::new_default_prog(),
        };
        if let Some(dir) = cwd.filter(|dir| dir.is_dir()).map(Path::to_path_buf).or_else(dirs::home_dir) {
            command.cwd(dir);
        }
        command.env("TERM", resolve_term(&config.term));
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "ionterm");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));

        let child = pair.slave.spawn_command(command).context("starting the shell")?;
        // The child holds its own handle; keeping ours open would stop us from
        // ever seeing EOF when it exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("cloning the PTY reader")?;
        let writer: SharedWriter = Arc::new(Mutex::new(pair.master.take_writer().context("taking the PTY writer")?));

        let reply_writer = writer.clone();
        std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut processor = Processor::new();
                let mut detector = zmodem::Detector::default();
                let mut session: Option<zmodem::Session> = None;
                let mut buf = vec![0u8; 64 * 1024];
                // Debugging aid: IONTERM_DUMP=<file> records the raw output.
                let mut dump = std::env::var_os("IONTERM_DUMP").and_then(|path| {
                    std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
                });
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            tracing::debug!("PTY read ended: {error}");
                            break;
                        }
                    };
                    if let Some(file) = &mut dump {
                        let _ = file.write_all(&buf[..n]);
                    }
                    let mut replies = Vec::new();
                    let mut data = Cow::Borrowed(&buf[..n]);
                    loop {
                        if let Some(active) = &mut session {
                            match active.feed(&data) {
                                // Over: the rest is the shell's again.
                                Some(rest) => {
                                    session = None;
                                    data = Cow::Owned(rest);
                                }
                                None => break,
                            }
                        }
                        let (shown, found) = detector.scan(&data);
                        if !shown.is_empty() {
                            let mut term = term.lock().expect("terminal lock poisoned");
                            processor.advance(&mut term, &shown);
                            replies.extend(term.take_output());
                        }
                        let Some(start) = found else { break };
                        let events = proxy.clone();
                        session = Some(zmodem::Session::new(start.role, reply_writer.clone(), zmodem.clone(), move |event| {
                            let _ = events.send_event(UserEvent::Zmodem(pane, event));
                        }));
                        data = Cow::Owned(start.bytes);
                    }
                    if !replies.is_empty() {
                        let mut writer = reply_writer.lock().expect("writer lock poisoned");
                        let _ = writer.write_all(&replies);
                        let _ = writer.flush();
                    }
                    if !wakeup_pending.swap(true, Ordering::AcqRel) && proxy.send_event(UserEvent::Wakeup(pane)).is_err() {
                        break;
                    }
                }
                if let Some(mut active) = session {
                    active.abandon();
                }
                let _ = proxy.send_event(UserEvent::ChildExited(pane));
            })
            .context("starting the PTY reader thread")?;

        Ok(Self {
            master: pair.master,
            writer,
            child,
        })
    }

    /// The PTY's input, shared with whoever else writes to it.
    pub fn writer(&self) -> SharedWriter {
        self.writer.clone()
    }

    pub fn write(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock().expect("writer lock poisoned");
        if let Err(error) = writer.write_all(bytes).and_then(|()| writer.flush()) {
            tracing::debug!("PTY write failed: {error}");
        }
    }

    /// The working directory of whatever runs in the foreground (the shell,
    /// or the program it started). Linux only; elsewhere new panes rely on
    /// the shell reporting its directory (OSC 7).
    pub fn current_dir(&self) -> Option<PathBuf> {
        #[cfg(target_os = "linux")]
        {
            let pid = self
                .master
                .process_group_leader()
                .map(i64::from)
                .or_else(|| self.child.process_id().map(i64::from))?;
            std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// The name of the program in the foreground (`fish`, `nvim`, `ssh`…),
    /// for the tab icon. Linux only.
    pub fn foreground_process(&self) -> Option<String> {
        let pid = self.foreground_pid()?;
        let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        Some(name.trim().to_string())
    }

    /// The foreground process group's leader. Linux only.
    pub fn foreground_pid(&self) -> Option<i64> {
        #[cfg(target_os = "linux")]
        {
            self.master
                .process_group_leader()
                .map(i64::from)
                .or_else(|| self.child.process_id().map(i64::from))
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// The child's exit code once it has exited.
    pub fn exit_code(&mut self) -> Option<u32> {
        // The reader saw EOF just before the child is reaped; wait briefly.
        for _ in 0..20 {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Some(status.exit_code());
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        None
    }

    pub fn resize(&self, size: PtySize) {
        if let Err(error) = self.master.resize(size) {
            tracing::warn!("PTY resize failed: {error}");
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Closing the window should not leave the shell running.
        let _ = self.child.kill();
    }
}

/// Splits a configured command line on whitespace. Quoting isn't supported
/// yet; a wrapper script covers anything more complex.
fn shell_words(line: &str) -> Option<(String, Vec<String>)> {
    let mut words = line.split_whitespace().map(String::from);
    let program = words.next()?;
    Some((program, words.collect()))
}

/// Picks the `TERM` value. `xterm-ionterm` is only used when the entry is
/// installed locally, so programs never see a terminal they can't look up.
/// Over SSH the shell integration takes care of hosts without it.
fn resolve_term(setting: &str) -> String {
    if setting != "auto" {
        return setting.to_string();
    }
    if cfg!(unix) && terminfo_installed("xterm-ionterm") {
        "xterm-ionterm".into()
    } else {
        "xterm-256color".into()
    }
}

/// Looks for a compiled terminfo entry where ncurses would.
fn terminfo_installed(name: &str) -> bool {
    let mut dirs = Vec::new();
    if let Ok(dir) = std::env::var("TERMINFO") {
        dirs.push(dir.into());
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".terminfo"));
    }
    if let Ok(list) = std::env::var("TERMINFO_DIRS") {
        dirs.extend(list.split(':').filter(|s| !s.is_empty()).map(Into::into));
    }
    dirs.extend(["/etc/terminfo", "/lib/terminfo", "/usr/share/terminfo", "/usr/lib/terminfo"].map(Into::into));

    let first = name.chars().next().unwrap_or('x');
    // ncurses uses either the first letter or its hex code as the subdirectory.
    let subdirs = [first.to_string(), format!("{:02x}", first as u32)];
    dirs.iter().any(|dir: &std::path::PathBuf| {
        subdirs
            .iter()
            .any(|sub| Path::new(dir).join(sub).join(name).is_file())
    })
}
