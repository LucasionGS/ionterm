//! Saving the window's tabs as they change and reopening them at startup
//! (`[session] restore`); see `crate::session` for the file.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::App;
use crate::layout::{Direction, SplitNode};
use crate::session::{self, SavedPane, SavedTab, Session};
use crate::tab::Tab;

/// How often the session is compared with what was last saved.
const SAVE_INTERVAL: Duration = Duration::from_secs(3);

impl App {
    /// Whether this window takes part: not with `-e` or `ionterm ssh`, which
    /// open a window for one thing.
    fn session_enabled(&self) -> bool {
        self.config.session.restore && self.config.terminal.command.is_empty() && self.config.terminal.startup_ssh.is_none()
    }

    /// The tabs of the last session, opened again, and which was active.
    pub(super) fn restore_session(&mut self, metrics: ionterm_render::CellMetrics) -> Option<(Vec<Tab>, usize)> {
        if !self.session_enabled() {
            return None;
        }
        let saved = session::load()?;
        let mut tabs = Vec::new();
        for tab in saved.tabs {
            let mut panes = Vec::new();
            for pane in &tab.panes {
                let connection = pane.ssh.as_deref().map(|alias| self.connection_for(alias));
                match self.spawn_pane(metrics, connection, pane.cwd.as_ref().map(PathBuf::from)) {
                    Ok(pane) => panes.push(pane),
                    Err(error) => tracing::warn!("couldn't reopen a pane: {error:#}"),
                }
            }
            let ids: Vec<_> = panes.iter().map(|p| p.id).collect();
            let Some(&first) = ids.first() else { continue };
            // A pane that didn't open breaks the saved tree: line them up instead.
            let tree = session::decode_tree(&tab.tree, &ids).unwrap_or_else(|| {
                let mut tree = SplitNode::Leaf(first);
                for pair in ids.windows(2) {
                    tree.split(pair[0], pair[1], Direction::Horizontal);
                }
                tree
            });
            let active = ids.get(tab.active).copied().unwrap_or(first);
            let mut restored = Tab::restored(panes, tree, active, tab.layout);
            restored.custom_title = tab.title;
            restored.color = tab.color.map(|[r, g, b]| ionterm_vt::Rgb { r, g, b });
            tabs.push(restored);
        }
        if tabs.is_empty() {
            return None;
        }
        let active = saved.active_tab.min(tabs.len() - 1);
        Some((tabs, active))
    }

    /// The window as a session: shells and SSH connections, not the
    /// settings page or panes running a program.
    fn session_snapshot(&self) -> Option<Session> {
        let w = self.window.as_ref()?;
        let mut tabs = Vec::new();
        let mut active_tab = 0;
        for (index, tab) in w.tabs.iter().enumerate() {
            if tab.settings.is_some() {
                continue;
            }
            let kept: Vec<_> = tab.panes.iter().filter(|p| !p.runs_command).collect();
            if kept.is_empty() {
                continue;
            }
            let mut tree = Some(tab.tree().clone());
            for pane in tab.panes.iter().filter(|p| p.runs_command) {
                tree = tree.and_then(|t| t.remove(pane.id));
            }
            let Some(tree) = tree else { continue };
            let index_of = |id| kept.iter().position(|p| p.id == id);
            let Some(encoded) = session::encode_tree(&tree, &index_of) else { continue };
            if index == w.active_tab {
                active_tab = tabs.len();
            }
            tabs.push(SavedTab {
                title: tab.custom_title.clone(),
                color: tab.color.map(|c| [c.r, c.g, c.b]),
                layout: tab.layout,
                active: index_of(tab.active).unwrap_or(0),
                tree: encoded,
                panes: kept
                    .iter()
                    .map(|pane| match &pane.connection {
                        Some(connection) => SavedPane {
                            cwd: None,
                            ssh: connection.managed.then(|| connection.alias.clone()),
                        },
                        None => SavedPane {
                            cwd: pane.working_dir().map(|dir| dir.display().to_string()),
                            ssh: None,
                        },
                    })
                    .collect(),
            });
        }
        Some(Session {
            pid: std::process::id(),
            active_tab,
            tabs,
        })
    }

    /// Saves the session when it changed; `force` skips the wait (the window
    /// is closing). Returns when to check again.
    pub(super) fn save_session(&mut self, now: Instant, force: bool) -> Option<Instant> {
        if !self.session_enabled() {
            return None;
        }
        if !force && now < self.session_due {
            return Some(self.session_due);
        }
        self.session_due = now + SAVE_INTERVAL;
        let snapshot = self.session_snapshot().filter(|s| !s.tabs.is_empty());
        if let Some(snapshot) = snapshot
            && self.session_saved.as_ref() != Some(&snapshot)
            && session::is_ours_to_save()
        {
            match session::save(&snapshot) {
                Ok(()) => self.session_saved = Some(snapshot),
                Err(error) => tracing::warn!("couldn't save the session: {error}"),
            }
        }
        Some(self.session_due)
    }
}
