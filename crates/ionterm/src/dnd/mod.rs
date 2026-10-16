//! Files dragged onto the window from a file manager.
//!
//! winit 0.30 reports file drops on X11, Windows and macOS, but not on
//! Wayland, so there ionterm listens for drags itself (see `wayland.rs`).
//! Both paths end up as `UserEvent::Drag`.

#[cfg(target_os = "linux")]
mod wayland;

use std::path::{Path, PathBuf};

use winit::event_loop::EventLoopProxy;
use winit::window::Window;

use crate::app::UserEvent;

#[cfg(target_os = "linux")]
pub use wayland::Listener;

/// Nothing to listen to where winit reports drops itself.
#[cfg(not(target_os = "linux"))]
pub struct Listener;

/// A drag over the window. Positions are in logical pixels from the
/// window's top-left corner; `None` means "wherever the pointer is".
#[derive(Debug)]
pub enum DragEvent {
    /// Files are being dragged over the window.
    Hover(Option<(f64, f64)>),
    /// The drag left the window without dropping.
    Left,
    Dropped {
        position: Option<(f64, f64)>,
        paths: Vec<PathBuf>,
    },
}

/// Names a window's Wayland surface the way drop events do, to find the
/// window a drop is over.
pub fn surface_key(window: &Window) -> Option<usize> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Wayland(handle) => Some(handle.surface.as_ptr() as usize),
        _ => None,
    }
}

/// Starts listening for drops where winit can't, which is Wayland. One
/// listener serves every window of the app.
pub fn listen(window: &Window, proxy: EventLoopProxy<UserEvent>) -> Option<Listener> {
    #[cfg(target_os = "linux")]
    {
        wayland::listen(window, proxy)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (window, proxy);
        None
    }
}

/// The local paths in a `text/uri-list` (RFC 2483): one URI per line,
/// `#` lines are comments, and only `file://` URIs name local files.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_uri_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(crate::pane::path_from_file_url)
        .collect()
}

/// Paths as they'd be typed in a shell: quoted only when they need it, and
/// separated by spaces, with one after the last so the next word can follow.
pub fn shell_words(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in paths {
        out.push_str(&quote_path(path));
        out.push(' ');
    }
    out
}

fn quote_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let safe = |c: char| c.is_alphanumeric() || "/._-+,:@%=~".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        return text.into_owned();
    }
    if cfg!(windows) {
        // cmd and PowerShell both take double quotes, and Windows paths
        // can't contain them.
        format!("\"{text}\"")
    } else {
        crate::files::remote::quote(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uri_lists() {
        let list = "# from a file manager\r\nfile:///home/ion/a%20b.txt\r\nhttps://example.com/x\r\nfile://localhost/tmp/c\r\n";
        assert_eq!(parse_uri_list(list), vec![PathBuf::from("/home/ion/a b.txt"), PathBuf::from("/tmp/c")]);
    }

    #[cfg(not(windows))]
    #[test]
    fn quotes_paths_for_the_shell() {
        let paths = [PathBuf::from("/tmp/plain-name.txt"), PathBuf::from("/tmp/it's here")];
        assert_eq!(shell_words(&paths), "/tmp/plain-name.txt '/tmp/it'\\''s here' ");
    }
}
