//! Editing a server's files locally (`crate::edit`): asking about `sudo`,
//! opening the editor tab, watching the local copy, writing it back on
//! every save, and telling a waiting `ionterm-edit` when it's done.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{App, UserEvent};
use crate::dialog::{self, Answer, Dialog};
use crate::edit::{self, Access, Event, RemoteState, Secret, Written};
use crate::files::remote::Remote;
use crate::hints::{self, OpenFile};
use crate::layout::PaneId;

/// Editors save in steps (a temporary file, then a rename): wait for them
/// to settle before writing back.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(200);

/// One file being edited.
pub(super) struct EditSession {
    id: u64,
    /// The pane whose connection reads and writes it, and whose
    /// `ionterm-edit` waits for it.
    requester: PaneId,
    /// What the waiting `ionterm-edit` listens for; `None` when the file
    /// panel opened it.
    nonce: Option<String>,
    remote: Remote,
    path: String,
    access: Access,
    /// The local copy, once read.
    local: Option<PathBuf>,
    /// The editor's pane, while it runs.
    editor: Option<PaneId>,
    /// The server's file as last read or written.
    known: Option<RemoteState>,
    /// The fingerprint of what the server has.
    sent: u64,
    save_at: Option<Instant>,
    saving: bool,
    /// The editor closed: finish once the last save is written.
    closing: bool,
    saves: usize,
    error: Option<String>,
    _watcher: Option<notify::RecommendedWatcher>,
}

impl EditSession {
    fn name(&self) -> String {
        edit::local_name(&self.path)
    }
}

/// What an edit dialog asks.
pub enum Question {
    /// Read and write the file with `sudo`?
    Sudo,
    Password,
    /// The file changed on the server: overwrite it?
    Conflict,
}

impl App {
    /// Opens a server's file in the local editor. `nonce` is set when
    /// `ionterm-edit` asked and waits for the answer.
    pub(super) fn start_edit(&mut self, requester: PaneId, remote: Remote, path: String, nonce: Option<String>) {
        let id = self.next_edit_id;
        self.next_edit_id += 1;
        self.edits.push(EditSession {
            id,
            requester,
            nonce,
            remote: remote.clone(),
            path: path.clone(),
            access: Access::User,
            local: None,
            editor: None,
            known: None,
            sent: 0,
            save_at: None,
            saving: false,
            closing: false,
            saves: 0,
            error: None,
            _watcher: None,
        });
        self.edit_work(id, move || Event::Probed(edit::probe(&remote, &path)));
    }

    /// Runs blocking work for an edit on a thread; its result comes back as
    /// a `UserEvent::Edit`.
    fn edit_work(&self, id: u64, job: impl FnOnce() -> Event + Send + 'static) {
        let proxy = self.proxy.clone();
        let spawned = std::thread::Builder::new().name(format!("edit-{id}")).spawn(move || {
            let _ = proxy.send_event(UserEvent::Edit(id, job()));
        });
        if let Err(error) = spawned {
            tracing::warn!("could not start editing: {error}");
        }
    }

    fn edit_mut(&mut self, id: u64) -> Option<&mut EditSession> {
        self.edits.iter_mut().find(|e| e.id == id)
    }

    pub(super) fn edit_event(&mut self, id: u64, event: Event) {
        let Some(session) = self.edit_mut(id) else { return };
        match event {
            Event::Probed(Ok(probe)) if probe.readable && probe.writable => self.edit_read(id),
            Event::Probed(Ok(_)) => self.edit_ask(id, Question::Sudo, None),
            Event::SudoChecked(Ok(false)) => {
                session.access = Access::Sudo(None);
                self.edit_read(id);
            }
            Event::SudoChecked(Ok(true)) => self.edit_ask(id, Question::Password, None),
            Event::PasswordChecked(Ok(password)) => {
                session.access = Access::Sudo(Some(password));
                self.edit_read(id);
            }
            Event::PasswordChecked(Err(message)) => self.edit_ask(id, Question::Password, Some(message)),
            Event::Read(Ok((content, state))) => self.edit_open(id, content, state),
            Event::Changed => {
                session.save_at = Some(Instant::now() + SAVE_DEBOUNCE);
            }
            Event::Written(fingerprint, result) => {
                session.saving = false;
                match result {
                    Ok(Written::Saved(state)) => {
                        session.known = Some(state);
                        session.sent = fingerprint;
                        session.saves += 1;
                        session.error = None;
                        let message = format!("Saved {} to {}", session.name(), session.remote.alias);
                        self.edit_toast(id, message);
                        // Saved again meanwhile, or closing: see what's left.
                        self.edit_save(id, false);
                    }
                    Ok(Written::Conflict(_)) => self.edit_ask(id, Question::Conflict, None),
                    Err(message) => {
                        let toast = format!("Couldn't save {}: {message}", session.name());
                        session.error = Some(message);
                        let closing = session.closing;
                        self.edit_toast(id, toast);
                        if closing {
                            self.edit_finish(id);
                        }
                    }
                }
            }
            Event::ServerCopy(Ok(content)) => self.edit_show_diff(id, content),
            Event::Probed(Err(message))
            | Event::SudoChecked(Err(message))
            | Event::Read(Err(message))
            | Event::ServerCopy(Err(message)) => {
                session.error = Some(message.clone());
                let toast = format!("Couldn't edit {}: {message}", session.name());
                self.edit_toast(id, toast);
                // Only a failed comparison leaves the editor open.
                if self.edit_mut(id).is_some_and(|s| s.editor.is_none()) {
                    self.edit_finish(id);
                }
            }
        }
    }

    fn edit_read(&mut self, id: u64) {
        let Some(session) = self.edit_mut(id) else { return };
        let (remote, path, access) = (session.remote.clone(), session.path.clone(), session.access.clone());
        self.edit_work(id, move || Event::Read(edit::read(&remote, &path, &access)));
    }

    /// Writes the local copy and opens it in the editor, in a tab next to
    /// the pane that asked.
    fn edit_open(&mut self, id: u64, content: Vec<u8>, state: RemoteState) {
        let proxy = self.proxy.clone();
        let Some(session) = self.edit_mut(id) else { return };
        let name = session.name();
        let local = match write_local(id, &name, &content) {
            Ok(local) => local,
            Err(error) => {
                session.error = Some(format!("Couldn't keep a local copy: {error}"));
                return self.edit_finish(id);
            }
        };
        session.known = Some(state);
        session.sent = edit::fingerprint(&content);
        session._watcher = watch(&local, move || {
            let _ = proxy.send_event(UserEvent::Edit(id, Event::Changed));
        });
        session.local = Some(local.clone());
        let (requester, title) = (session.requester, format!("✎ {}:{name}", session.remote.alias));

        self.select_window_of_pane(requester);
        let cwd = local.parent().map(PathBuf::from);
        match hints::open_file_command("", &local, None, None) {
            Some(OpenFile::Tab(argv)) => {
                self.open_command_tab(argv, cwd);
                let Some(w) = &mut self.window else { return };
                let tab = &mut w.tabs[w.active_tab];
                tab.custom_title = Some(title);
                let editor = tab.active;
                if let Some(session) = self.edit_mut(id) {
                    session.editor = Some(editor);
                }
            }
            // A graphical editor: saves are written back until ionterm quits,
            // but there's no knowing when it's closed, so nobody waits.
            Some(OpenFile::Detached(argv)) => {
                let started = std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                if let Err(error) = started {
                    let message = format!("Couldn't run {}: {error}", argv[0]);
                    if let Some(session) = self.edit_mut(id) {
                        session.error = Some(message);
                    }
                    return self.edit_finish(id);
                }
                self.edit_reply(id, "opened");
            }
            None => {
                if let Some(session) = self.edit_mut(id) {
                    session.error = Some("There's no editor to open it in: set $EDITOR.".into());
                }
                self.edit_finish(id);
            }
        }
    }

    /// Writes the local copy back if it changed since the last write;
    /// `force` writes even over changes made on the server. Finishes a
    /// closing edit with nothing left to write.
    fn edit_save(&mut self, id: u64, force: bool) {
        let Some(session) = self.edit_mut(id) else { return };
        session.save_at = None;
        if session.saving {
            return;
        }
        let content = session.local.as_ref().and_then(|p| std::fs::read(p).ok());
        let Some(content) = content else {
            if session.closing {
                self.edit_finish(id);
            }
            return;
        };
        let fingerprint = edit::fingerprint(&content);
        if fingerprint == session.sent && !force {
            if session.closing {
                self.edit_finish(id);
            }
            return;
        }
        session.saving = true;
        let expected = if force { None } else { session.known.clone() };
        let (remote, path, access) = (session.remote.clone(), session.path.clone(), session.access.clone());
        self.edit_work(id, move || Event::Written(fingerprint, edit::write(&remote, &path, content, expected.as_ref(), &access)));
    }

    /// Saves that settled, and editors whose tab was closed rather than
    /// quit; returns when to look again.
    pub(super) fn edit_timers(&mut self, now: Instant) -> Option<Instant> {
        let exists = |app: &Self, pane: PaneId| {
            let in_tabs = |tabs: &[crate::tab::Tab]| tabs.iter().any(|t| t.pane(pane).is_some());
            app.windows().any(|w| in_tabs(&w.tabs))
                || app.pending_windows.iter().any(|n| matches!(&n.content, super::Content::Tabs(tabs, _) if in_tabs(tabs)))
        };
        let gone: Vec<PaneId> = self.edits.iter().filter_map(|e| e.editor).filter(|&p| !exists(self, p)).collect();
        for pane in gone {
            self.edit_pane_closed(pane);
        }
        let due: Vec<u64> = self.edits.iter().filter(|e| e.save_at.is_some_and(|at| at <= now)).map(|e| e.id).collect();
        for id in due {
            self.edit_save(id, false);
        }
        self.edits.iter().filter_map(|e| e.save_at).min()
    }

    /// A pane closed: if it was an editor, write back what's left and tell
    /// whoever waits.
    pub(super) fn edit_pane_closed(&mut self, pane: PaneId) {
        let Some(session) = self.edits.iter_mut().find(|e| e.editor == Some(pane)) else { return };
        session.editor = None;
        session.closing = true;
        let id = session.id;
        self.edit_save(id, false);
    }

    /// Asks something in the window of the pane that started the edit.
    fn edit_ask(&mut self, id: u64, question: Question, error: Option<String>) {
        let Some(session) = self.edit_mut(id) else { return };
        let (name, host, closing) = (session.name(), session.remote.alias.clone(), session.closing);
        let pane = session.editor.unwrap_or(session.requester);
        if !self.select_window_of_pane(pane) {
            if let Some(session) = self.edit_mut(id) {
                session.error = Some("Its window closed.".into());
            }
            return self.edit_finish(id);
        }
        let purpose = super::files::Purpose::Edit(id, question);
        let dialog = match &purpose {
            super::files::Purpose::Edit(_, Question::Sudo) => Dialog::new(
                format!("Edit {name} with sudo?"),
                format!("You can't change {name} on {host} as yourself. ionterm can read and save it with sudo instead."),
                vec![dialog::Button::new("Use sudo", 's'), dialog::Button::new("Cancel", 'c')],
                purpose,
            ),
            super::files::Purpose::Edit(_, Question::Password) => Dialog::new(
                format!("sudo password on {host}"),
                "ionterm keeps it in memory only, until you close the file.",
                vec![dialog::Button::new("OK", 'o'), dialog::Button::new("Cancel", 'c')],
                purpose,
            )
            .with_secret_input(),
            _ => {
                let mut buttons = vec![dialog::Button::new("Overwrite", 'o').danger()];
                if !closing {
                    buttons.push(dialog::Button::new("Show changes", 'd'));
                }
                buttons.push(dialog::Button::new("Don't save", 'n'));
                let mut d = Dialog::new(
                    format!("{name} changed on {host}"),
                    "Someone changed it on the server since you opened it. Overwrite their changes with yours?",
                    buttons,
                    purpose,
                );
                d.default = d.buttons.len() - 1;
                d
            }
        };
        let Some(w) = &mut self.window else { return };
        w.dialog = Some(Dialog { error, ..dialog });
        w.window.request_redraw();
    }

    /// The answer to an edit dialog.
    pub(super) fn edit_answer(&mut self, id: u64, question: Question, answer: Answer) {
        let Some(session) = self.edit_mut(id) else { return };
        let remote = session.remote.clone();
        match (question, answer.button) {
            (Question::Sudo, Some(0)) => self.edit_work(id, move || Event::SudoChecked(edit::sudo_needs_password(&remote))),
            (Question::Password, Some(0)) if !answer.text.is_empty() => {
                let password = Secret(answer.text);
                self.edit_work(id, move || Event::PasswordChecked(edit::check_sudo_password(&remote, password)));
            }
            (Question::Password, Some(0)) => self.edit_ask(id, Question::Password, Some("Type the password first.".into())),
            (Question::Conflict, Some(0)) => self.edit_save(id, true),
            (Question::Conflict, Some(1)) if !session.closing => {
                let (path, access) = (session.path.clone(), session.access.clone());
                self.edit_work(id, move || Event::ServerCopy(edit::read(&remote, &path, &access).map(|(content, _)| content)));
            }
            // Not saved: the editor keeps the changes; saving again asks again.
            (Question::Conflict, _) => {
                if session.closing {
                    session.error = Some("Not saved: it changed on the server.".into());
                    self.edit_finish(id);
                }
            }
            (Question::Sudo | Question::Password, _) => {
                session.error = Some("Cancelled.".into());
                self.edit_finish(id);
            }
        }
    }

    /// Opens a tab comparing the server's copy with yours.
    fn edit_show_diff(&mut self, id: u64, server: Vec<u8>) {
        let Some(session) = self.edit_mut(id) else { return };
        let Some(local) = session.local.clone() else { return };
        let theirs = local.with_file_name(format!("{}.server", session.name()));
        if let Err(error) = std::fs::write(&theirs, server) {
            let message = format!("Couldn't compare: {error}");
            return self.edit_toast(id, message);
        }
        let argv = vec![
            "sh".to_string(),
            "-c".into(),
            "diff -u -- \"$1\" \"$2\" | less -R".into(),
            "sh".into(),
            theirs.to_string_lossy().into_owned(),
            local.to_string_lossy().into_owned(),
        ];
        let title = format!("Changes: {}", session.name());
        let cwd = local.parent().map(PathBuf::from);
        self.open_command_tab(argv, cwd);
        if let Some(w) = &mut self.window {
            let tab = w.active_tab;
            w.tabs[tab].custom_title = Some(title);
            w.show_toast("Save again in the editor to choose what to keep");
        }
    }

    fn edit_toast(&mut self, id: u64, message: String) {
        let Some(session) = self.edits.iter().find(|e| e.id == id) else { return };
        let pane = session.editor.unwrap_or(session.requester);
        if self.select_window_of_pane(pane)
            && let Some(w) = &mut self.window
        {
            w.show_toast(message);
        }
    }

    /// Tells a waiting `ionterm-edit` how it went: it reads a line
    /// starting with its nonce (typed into its pane, with echo off).
    fn edit_reply(&mut self, id: u64, status: &str) {
        let Some(session) = self.edit_mut(id) else { return };
        let Some(nonce) = session.nonce.take() else { return };
        let requester = session.requester;
        // One line, no control characters: it's typed into a terminal.
        let status: String = status.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        if self.select_window_of_pane(requester)
            && let Some(pane) = self.window.as_ref().and_then(|w| w.tabs.iter().find_map(|t| t.pane(requester)))
        {
            // Stopped waiting (ctrl+c) and back at the prompt: typed there,
            // the answer would run as a command. Shell integration tells.
            let at_prompt = pane.command.is_none() && !pane.lock().prompt_lines().is_empty();
            if !at_prompt {
                pane.write(format!("{nonce} {status}\r").as_bytes());
            }
        }
    }

    /// Ends an edit: answers `ionterm-edit` and removes the local copy.
    fn edit_finish(&mut self, id: u64) {
        let Some(session) = self.edits.iter().find(|e| e.id == id) else { return };
        let status = match (&session.error, session.saves) {
            (Some(error), _) => error.clone(),
            (None, 0) => "unchanged".into(),
            (None, _) => "saved".into(),
        };
        self.edit_reply(id, &status);
        let Some(index) = self.edits.iter().position(|e| e.id == id) else { return };
        let session = self.edits.remove(index);
        if let Some(dir) = session.local.as_ref().and_then(|p| p.parent()) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Puts the content in a private folder of its own, readable only by you.
fn write_local(id: u64, name: &str, content: &[u8]) -> std::io::Result<PathBuf> {
    let path = edit::local_dir(id)?.join(name);
    std::fs::write(&path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// Calls `changed` when the file at `path` is written or replaced. The
/// folder is watched, since editors often save by renaming a new file over
/// the old one.
fn watch(path: &std::path::Path, changed: impl Fn() + Send + 'static) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;

    let dir = path.parent()?.to_path_buf();
    let name = path.file_name()?.to_os_string();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else { return };
        if event.kind.is_access() {
            return;
        }
        if event.paths.iter().any(|p| p.file_name() == Some(name.as_os_str())) {
            changed();
        }
    })
    .map_err(|error| tracing::warn!("can't watch the edited file: {error}"))
    .ok()?;
    watcher
        .watch(&dir, notify::RecursiveMode::NonRecursive)
        .map_err(|error| tracing::warn!("can't watch the edited file: {error}"))
        .ok()?;
    Some(watcher)
}
