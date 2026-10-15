//! The window's side of ZMODEM transfers (`sz` and `rz` in a pane): their
//! entries in the transfer list, the file picker for `rz`, and cancelling.

use std::io::Write;
use std::path::PathBuf;

use super::{App, TermWindow};
use crate::files;
use crate::files::transfer::{External, TransferEvent};
use crate::layout::PaneId;
use crate::pane::Pane;
use crate::zmodem::{self, Event};

impl TermWindow {
    fn pane_by_id(&self, id: PaneId) -> Option<&Pane> {
        self.tabs.iter().flat_map(|t| &t.panes).find(|p| p.id == id)
    }

    /// Stops a pane's ZMODEM transfer: the session stops, and the other
    /// side is told to give up.
    pub(super) fn cancel_zmodem(&mut self, pane: PaneId) {
        if let Some(&id) = self.zmodem_transfers.get(&pane) {
            // Its abort does the rest.
            self.transfers.cancel(id);
        } else if let Some(pane) = self.pane_by_id(pane) {
            pane.zmodem.cancel(|bytes| pane.write_raw(bytes));
        }
        self.window.request_redraw();
    }
}

impl App {
    pub(super) fn zmodem_event(&mut self, pane: PaneId, event: Event) {
        let proxy = self.proxy.clone();
        let Some(w) = &mut self.window else { return };
        w.window.request_redraw();
        match event {
            Event::NeedFiles => {
                w.show_toast("rz is waiting: choose files to send");
                files::pick_for_zmodem(proxy, pane);
            }
            Event::FileStarted {
                direction,
                name,
                local_path,
                size,
            } => {
                let Some(p) = w.pane_by_id(pane) else { return };
                let (shared, writer) = (p.zmodem.clone(), p.pty_writer());
                let external = External {
                    direction,
                    name,
                    local_path,
                    total: size,
                    method: "ZMODEM",
                };
                let id = w.transfers.add_external(pane, external, move || {
                    shared.cancel(|bytes| {
                        let mut writer = writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        let _ = writer.write_all(bytes).and_then(|()| writer.flush());
                    });
                });
                w.zmodem_transfers.insert(pane, id);
            }
            Event::Progress(bytes) => {
                if let Some(&id) = w.zmodem_transfers.get(&pane) {
                    w.transfers.event(id, TransferEvent::Progress(bytes, None));
                }
            }
            Event::FileFinished(result) => {
                if let Some(id) = w.zmodem_transfers.remove(&pane) {
                    self.transfer_event(id, TransferEvent::Finished(result));
                }
            }
            Event::Ended(error) => {
                if let Some(id) = w.zmodem_transfers.remove(&pane) {
                    let result = Err(error.clone().unwrap_or_else(|| "Interrupted.".into()));
                    self.transfer_event(id, TransferEvent::Finished(result));
                }
                let Some(w) = &mut self.window else { return };
                match error {
                    Some(error) if error != "Cancelled." => w.show_toast(format!("ZMODEM transfer failed: {error}")),
                    _ => {}
                }
            }
        }
    }

    /// Files chosen for a waiting `rz`; none means the picker was closed.
    pub(super) fn zmodem_picked(&mut self, pane: PaneId, paths: Vec<PathBuf>) {
        let Some(w) = &mut self.window else { return };
        if paths.is_empty() {
            w.cancel_zmodem(pane);
            return;
        }
        let Some(p) = w.pane_by_id(pane) else { return };
        p.zmodem.queue_files(paths);
        // `rz` answers with ZRINIT straight away, and the session goes on.
        p.write_raw(&zmodem::wake_receiver());
    }

    /// Hands files to a pane's `rz`: now, when it's already waiting, or to
    /// the next one started there.
    pub(super) fn send_with_zmodem(&mut self, pane: PaneId, paths: Vec<PathBuf>) {
        let Some(w) = &mut self.window else { return };
        let Some(p) = w.pane_by_id(pane) else { return };
        let what = match paths.as_slice() {
            [one] => one.file_name().map_or_else(|| "1 file".into(), |n| n.to_string_lossy().into_owned()),
            many => format!("{} files", many.len()),
        };
        let waiting = p.zmodem.is_waiting();
        p.zmodem.queue_files(paths);
        if waiting {
            p.write_raw(&zmodem::wake_receiver());
            w.show_toast(format!("Sending {what} with rz"));
        } else {
            w.show_toast(format!("Run rz on the server to receive {what}"));
        }
    }
}
