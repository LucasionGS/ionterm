//! Files dragged onto the window: an SSH pane (or its file panel) uploads
//! them, a local pane gets their paths typed in, like most terminals do.

use std::path::PathBuf;

use ionterm_render::{ChromeItem, text_columns};
use winit::dpi::PhysicalPosition;

use super::{App, TermWindow};
use crate::dnd::{self, DragEvent};
use crate::files::remote::Remote;
use crate::input;
use crate::layout::{PaneId, Rect};
use crate::tabbar::{mix, truncate};
use crate::theme::Theme;

/// What dropping at a point does.
enum Target {
    Upload {
        pane: PaneId,
        remote: Remote,
        /// `None` = the remote home, when the shell hasn't said where it is.
        dir: Option<String>,
        rect: Rect,
    },
    Paste {
        pane: PaneId,
        rect: Rect,
    },
    /// Hand the files to `rz`, running or yet to be run.
    Zmodem {
        pane: PaneId,
        waiting: bool,
        rect: Rect,
    },
    /// Dropping here does nothing, for this reason.
    Refused {
        reason: &'static str,
        rect: Rect,
    },
}

impl Target {
    fn rect(&self) -> Rect {
        match self {
            Self::Upload { rect, .. } | Self::Paste { rect, .. } | Self::Zmodem { rect, .. } | Self::Refused { rect, .. } => *rect,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Upload { remote, dir, .. } => format!("Upload to {}:{}", remote.alias, dir.as_deref().unwrap_or("~")),
            Self::Paste { .. } => "Paste the paths".into(),
            Self::Zmodem { waiting: true, .. } => "Send to rz".into(),
            Self::Zmodem { waiting: false, .. } => "Send with rz (ZMODEM)".into(),
            Self::Refused { reason, .. } => (*reason).into(),
        }
    }
}

impl TermWindow {
    /// What dropping at a point (in physical pixels) of the active tab does.
    fn drop_target(&self, x: f64, y: f64) -> Option<Target> {
        let tab = &self.tabs[self.active_tab];
        if let Some(panel) = &tab.files
            && panel.rect.contains(x as f32, y as f32)
        {
            return Some(Target::Upload {
                pane: panel.pane,
                remote: panel.remote.clone(),
                dir: panel.dir().map(String::from),
                rect: panel.rect,
            });
        }
        let id = self.pane_at(x, y).unwrap_or(tab.active);
        let pane = tab.pane(id)?;
        let rect = pane.area;
        Some(match &pane.connection {
            _ if pane.disconnected.is_some() => Target::Refused {
                reason: "The connection is closed",
                rect,
            },
            _ if pane.zmodem.is_waiting() => Target::Zmodem {
                pane: id,
                waiting: true,
                rect,
            },
            _ if pane.zmodem.is_active() => Target::Refused {
                reason: "A transfer is running here",
                rect,
            },
            Some(connection) if connection.managed => Target::Upload {
                pane: id,
                remote: Remote::new(connection),
                dir: pane.remote_cwd.clone(),
                rect,
            },
            // A typed `ssh`: no shared connection to upload through, but
            // `rz` on the server can take the files over the terminal.
            Some(_) => Target::Zmodem {
                pane: id,
                waiting: false,
                rect,
            },
            None if pane.detected_ssh.is_some() => Target::Zmodem {
                pane: id,
                waiting: false,
                rect,
            },
            None => Target::Paste { pane: id, rect },
        })
    }

    /// Highlights where dragged files would go.
    pub(super) fn draw_drag(&self, theme: &Theme, out: &mut Vec<ChromeItem>) {
        let Some(position) = self.file_drag else { return };
        let Some(target) = self.drop_target(position.x, position.y) else { return };
        let m = self.renderer.cell_metrics();
        let scale = self.scale_factor as f32;
        let bg = theme.background.0;
        let fg = theme.foreground.0;
        let accent = match target {
            Target::Refused { .. } => mix(fg, bg, 0.4),
            _ => theme.cursor.0,
        };
        let r = target.rect();
        out.push(ChromeItem::Rect {
            rect: [r.x, r.y, r.width, r.height],
            color: accent,
            alpha: 0.12,
        });
        let edge = (2.0 * scale).round().max(1.0);
        for [x, y, w, h] in [
            [r.x, r.y, r.width, edge],
            [r.x, r.y + r.height - edge, r.width, edge],
            [r.x, r.y, edge, r.height],
            [r.x + r.width - edge, r.y, edge, r.height],
        ] {
            out.push(ChromeItem::Rect {
                rect: [x, y, w, h],
                color: accent,
                alpha: 1.0,
            });
        }
        // The label, in a pill in the middle.
        let max_cols = ((r.width / m.width) as usize).saturating_sub(6).max(8);
        let text = truncate(&target.label(), max_cols);
        let width = (text_columns(&text) as f32 + 3.0) * m.width;
        let height = (m.height * 1.9).round();
        let x = (r.x + (r.width - width) / 2.0).round();
        let y = (r.y + (r.height - height) / 2.0).round();
        out.push(ChromeItem::RoundedRect {
            rect: [x, y, width, height],
            radius: (height / 2.0).round(),
            top_only: false,
            color: mix(bg, accent, 0.25),
            alpha: 1.0,
        });
        out.push(ChromeItem::Text {
            text,
            x: x + 1.5 * m.width,
            y: y + ((height - m.height) / 2.0).round(),
            color: fg,
            bold: true,
        });
    }
}

impl App {
    /// A drag moved, left, or dropped files (from the Wayland listener).
    pub(super) fn drag_event(&mut self, event: DragEvent) {
        let Some(w) = &mut self.window else { return };
        let scale = w.scale_factor;
        let physical = |p: Option<(f64, f64)>| p.map(|(x, y)| PhysicalPosition::new(x * scale, y * scale));
        match event {
            DragEvent::Hover(position) => {
                w.file_drag = physical(position).or(Some(w.pointer));
                w.window.request_redraw();
            }
            DragEvent::Left => {
                if w.file_drag.take().is_some() {
                    w.window.request_redraw();
                }
            }
            DragEvent::Dropped { position, paths } => {
                let position = physical(position);
                self.drop_files(position, paths);
            }
        }
    }

    /// Uploads or pastes dropped files, depending on where they landed
    /// (`None` = at the pointer).
    pub(super) fn drop_files(&mut self, position: Option<PhysicalPosition<f64>>, paths: Vec<PathBuf>) {
        let Some(w) = &mut self.window else { return };
        w.file_drag = None;
        w.window.request_redraw();
        if paths.is_empty() {
            return;
        }
        let position = position.unwrap_or(w.pointer);
        let Some(target) = w.drop_target(position.x, position.y) else { return };
        match target {
            Target::Upload { pane, remote, dir, .. } => {
                let count = paths.len();
                let what = if count == 1 {
                    paths[0].file_name().map_or_else(|| "1 item".into(), |n| n.to_string_lossy().into_owned())
                } else {
                    format!("{count} items")
                };
                w.show_toast(format!("Uploading {what} to {}:{}", remote.alias, dir.as_deref().unwrap_or("~")));
                self.upload(pane, remote, dir, paths);
            }
            Target::Paste { pane, .. } => {
                let tab = w.active_tab;
                let Some(pane) = w.tabs[tab].pane(pane) else { return };
                let mode = pane.lock().mode();
                pane.write(&input::encode_paste(&dnd::shell_words(&paths), mode));
                pane.lock().scroll_to_bottom();
            }
            Target::Zmodem { pane, .. } => {
                let (files, folders): (Vec<_>, Vec<_>) = paths.into_iter().partition(|p| !p.is_dir());
                if !folders.is_empty() {
                    w.show_toast("Folders can't be sent with rz; connect with ctrl+shift+s to upload them");
                }
                if !files.is_empty() {
                    self.send_with_zmodem(pane, files);
                }
            }
            Target::Refused { reason, .. } => w.show_toast(reason),
        }
    }

    /// Hands over the files winit reported one by one (X11, Windows).
    pub(super) fn flush_dropped(&mut self) {
        let Some(w) = &mut self.window else { return };
        if w.dropped.is_empty() {
            return;
        }
        let paths = std::mem::take(&mut w.dropped);
        self.drag_event(DragEvent::Dropped { position: None, paths });
    }
}
