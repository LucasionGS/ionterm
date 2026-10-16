//! Remote files for SSH tabs: the side panel, uploads and downloads.
//!
//! Everything that touches the network runs on a background thread and
//! reports back as a `UserEvent`, so a slow server never stalls the window.

pub mod panel;
pub mod remote;
mod rsync;
pub mod transfer;

use std::collections::HashSet;
use std::path::PathBuf;

use winit::event_loop::EventLoopProxy;

use crate::app::UserEvent;
use crate::layout::PaneId;
use remote::{Listing, Remote};

/// Results of background file work.
#[derive(Debug)]
pub enum FilesEvent {
    Listed {
        pane: PaneId,
        request: u64,
        result: Result<Listing, String>,
    },
    /// A rename, delete or new folder finished.
    Changed {
        pane: PaneId,
        result: Result<String, String>,
        /// Select this name in the refreshed listing.
        select: Option<String>,
    },
    /// The names already in an upload's destination folder, to find
    /// conflicts and free names.
    UploadTargets {
        pane: PaneId,
        dir: String,
        paths: Vec<PathBuf>,
        existing: Result<HashSet<String>, String>,
    },
    /// Local files chosen in the file picker, to upload to `dir`.
    Picked {
        pane: PaneId,
        dir: String,
        paths: Vec<PathBuf>,
    },
    /// Files chosen for a waiting `rz`; empty when the picker was closed.
    ZmodemPicked { pane: PaneId, paths: Vec<PathBuf> },
}

impl FilesEvent {
    /// The pane whose panel or transfer it's about.
    pub fn pane(&self) -> PaneId {
        match self {
            Self::Listed { pane, .. }
            | Self::Changed { pane, .. }
            | Self::UploadTargets { pane, .. }
            | Self::Picked { pane, .. }
            | Self::ZmodemPicked { pane, .. } => *pane,
        }
    }
}

fn send(proxy: &EventLoopProxy<UserEvent>, event: FilesEvent) {
    let _ = proxy.send_event(UserEvent::Files(event));
}

fn spawn(name: &str, job: impl FnOnce() + Send + 'static) {
    if let Err(error) = std::thread::Builder::new().name(name.into()).spawn(job) {
        tracing::warn!("could not start {name}: {error}");
    }
}

pub fn list(proxy: EventLoopProxy<UserEvent>, pane: PaneId, request: u64, remote: Remote, dir: Option<String>) {
    spawn("files-list", move || {
        let result = remote.list(dir.as_deref());
        send(&proxy, FilesEvent::Listed { pane, request, result });
    });
}

/// Runs a change (rename, delete, mkdir) and reports it with a message.
pub fn change(
    proxy: EventLoopProxy<UserEvent>,
    pane: PaneId,
    select: Option<String>,
    job: impl FnOnce() -> Result<String, String> + Send + 'static,
) {
    spawn("files-change", move || {
        let result = job();
        send(&proxy, FilesEvent::Changed { pane, result, select });
    });
}

/// Looks up what's already in `dir` (`None` = the remote home) before
/// uploading `paths` there.
pub fn check_upload(proxy: EventLoopProxy<UserEvent>, pane: PaneId, remote: Remote, dir: Option<String>, paths: Vec<PathBuf>) {
    spawn("files-check", move || {
        let names: Vec<String> = paths
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        // The whole folder's names, so "rename" can pick a free one.
        let (dir, existing) = match (remote.list(dir.as_deref()), dir) {
            (Ok(listing), _) => (listing.path, Ok(listing.entries.into_iter().map(|e| e.name).collect())),
            (Err(_), Some(dir)) => {
                let found = remote.existing(&dir, &names).map(|found| found.into_iter().collect());
                (dir, found)
            }
            (Err(error), None) => ("~".into(), Err(error)),
        };
        send(&proxy, FilesEvent::UploadTargets { pane, dir, paths, existing });
    });
}

/// Opens the system's file picker (the XDG portal on Linux) for files or
/// folders to upload into `dir`.
pub fn pick(proxy: EventLoopProxy<UserEvent>, pane: PaneId, dir: String, folders: bool) {
    spawn("files-pick", move || {
        let dialog = rfd::FileDialog::new().set_title(if folders {
            "Upload folders"
        } else {
            "Upload files"
        });
        let paths = if folders { dialog.pick_folders() } else { dialog.pick_files() };
        if let Some(paths) = paths.filter(|p| !p.is_empty()) {
            send(&proxy, FilesEvent::Picked { pane, dir, paths });
        }
    });
}

/// Asks which files a waiting `rz` should receive.
pub fn pick_for_zmodem(proxy: EventLoopProxy<UserEvent>, pane: PaneId) {
    spawn("files-pick", move || {
        let paths = rfd::FileDialog::new().set_title("Send with rz").pick_files().unwrap_or_default();
        send(&proxy, FilesEvent::ZmodemPicked { pane, paths });
    });
}

/// Shows a local file or folder in the file manager.
pub fn reveal(path: &std::path::Path) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    }
    #[cfg(not(windows))]
    {
        // Open the containing folder; file managers differ on selecting.
        let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        crate::links::open(&file_url(dir));
    }
}

/// A `file://` URL for a local path, with unsafe bytes %-escaped.
#[cfg(not(windows))]
fn file_url(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut url = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}
