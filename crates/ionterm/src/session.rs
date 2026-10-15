//! Session restore (`[session] restore`): the window's tabs, splits,
//! directories and SSH connections are saved as they change and reopened
//! next time. Closing the last tab yourself ends the session; closing the
//! window (or logging out) keeps it. With several windows open, the one
//! that saved first owns the session until it ends; the others leave it be.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::layout::{Direction, Layout, PaneId, SplitNode};

const FILE: &str = "session.toml";

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Session {
    /// The ionterm that saved it: a session still open isn't restored twice.
    pub pid: u32,
    pub active_tab: usize,
    #[serde(default)]
    pub tabs: Vec<SavedTab>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SavedTab {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 3]>,
    #[serde(default)]
    pub layout: Layout,
    /// Index of the active pane in `panes`.
    #[serde(default)]
    pub active: usize,
    /// The split tree over `panes` indices, as `encode_tree` writes it.
    pub tree: String,
    pub panes: Vec<SavedPane>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct SavedPane {
    /// Where its shell was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The ssh config host it was connected to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
}

fn path() -> Option<PathBuf> {
    let dir = dirs::state_dir().or_else(dirs::data_local_dir)?;
    Some(dir.join("ionterm").join(FILE))
}

/// The saved session, unless there's none, it's empty, or the ionterm that
/// saved it is still running.
pub fn load() -> Option<Session> {
    let text = std::fs::read_to_string(path()?).ok()?;
    let session: Session = match toml::from_str(&text) {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!("ignoring the saved session: {error}");
            return None;
        }
    };
    if session.tabs.is_empty() || (session.pid != std::process::id() && is_running(session.pid)) {
        return None;
    }
    Some(session)
}

/// Whether this process may write the session: nobody else's live
/// ionterm saved the one there.
pub fn is_ours_to_save() -> bool {
    let Some(path) = path() else { return false };
    let owner = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<Session>(&text).ok())
        .map(|s| s.pid);
    owner.is_none_or(|pid| pid == std::process::id() || !is_running(pid))
}

/// Writes the session as TOML, through a temporary file so a crash
/// mid-write can't leave half of one.
pub fn save(session: &Session) -> std::io::Result<()> {
    let path = path().ok_or_else(|| std::io::Error::other("no state directory"))?;
    let text = toml::to_string(session).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("toml.new");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, &path)
}

/// Ends the session this process saved (its last tab was closed).
pub fn remove_ours() {
    let Some(path) = path() else { return };
    let ours = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| toml::from_str::<Session>(&text).ok())
        .is_some_and(|s| s.pid == std::process::id());
    if ours {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(target_os = "linux")]
fn is_running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| comm.trim() == "ionterm")
}

#[cfg(not(target_os = "linux"))]
fn is_running(_pid: u32) -> bool {
    false
}

/// The split tree as text, with leaves as indices: `h0.50(0,v0.30(1,2))`
/// is pane 0 on the left of panes 1 over 2.
pub fn encode_tree(node: &SplitNode, index_of: &dyn Fn(PaneId) -> Option<usize>) -> Option<String> {
    Some(match node {
        SplitNode::Leaf(id) => index_of(*id)?.to_string(),
        SplitNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let d = if *direction == Direction::Horizontal { 'h' } else { 'v' };
            format!("{d}{ratio:.3}({},{})", encode_tree(first, index_of)?, encode_tree(second, index_of)?)
        }
    })
}

/// Reads `encode_tree`'s text back with the panes' new ids. None unless
/// every pane appears exactly once.
pub fn decode_tree(text: &str, ids: &[PaneId]) -> Option<SplitNode> {
    let mut rest = text.trim();
    let mut seen = vec![false; ids.len()];
    let tree = parse_node(&mut rest, ids, &mut seen)?;
    (rest.is_empty() && seen.iter().all(|&s| s)).then_some(tree)
}

fn parse_node(rest: &mut &str, ids: &[PaneId], seen: &mut [bool]) -> Option<SplitNode> {
    let direction = match rest.chars().next()? {
        'h' => Direction::Horizontal,
        'v' => Direction::Vertical,
        _ => {
            let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
            let index: usize = rest[..digits].parse().ok()?;
            *rest = &rest[digits..];
            if std::mem::replace(seen.get_mut(index)?, true) {
                return None;
            }
            return Some(SplitNode::Leaf(ids[index]));
        }
    };
    let open = rest.find('(')?;
    let ratio: f32 = rest[1..open].parse().ok()?;
    *rest = &rest[open + 1..];
    let first = parse_node(rest, ids, seen)?;
    *rest = rest.strip_prefix(',')?;
    let second = parse_node(rest, ids, seen)?;
    *rest = rest.strip_prefix(')')?;
    Some(SplitNode::Split {
        direction,
        ratio: ratio.clamp(0.1, 0.9),
        first: Box::new(first),
        second: Box::new(second),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trees_round_trip() {
        let mut tree = SplitNode::Leaf(10);
        tree.split(10, 11, Direction::Horizontal);
        tree.split(11, 12, Direction::Vertical);
        let ids = [10, 11, 12];
        let text = encode_tree(&tree, &|id| ids.iter().position(|&i| i == id)).unwrap();
        assert_eq!(text, "h0.500(0,v0.500(1,2))");
        assert_eq!(decode_tree(&text, &ids), Some(tree));
        // Renumbered panes, and broken or incomplete trees.
        assert!(matches!(decode_tree("h0.3(1,0)", &[5, 6]), Some(SplitNode::Split { .. })));
        assert_eq!(decode_tree("h0.5(0,0)", &[1, 2]), None);
        assert_eq!(decode_tree("h0.5(0,1", &[1, 2]), None);
        assert_eq!(decode_tree("0", &[1, 2]), None);
        assert_eq!(decode_tree("3", &[1]), None);
    }

    #[test]
    fn sessions_round_trip_as_toml() {
        let session = Session {
            pid: 42,
            active_tab: 1,
            tabs: vec![
                SavedTab {
                    title: Some("logs".into()),
                    color: Some([200, 100, 50]),
                    layout: Layout::Tall,
                    active: 1,
                    tree: "h0.500(0,1)".into(),
                    panes: vec![
                        SavedPane {
                            cwd: Some("/tmp".into()),
                            ssh: None,
                        },
                        SavedPane {
                            cwd: None,
                            ssh: Some("ionserver".into()),
                        },
                    ],
                },
                SavedTab {
                    title: None,
                    color: None,
                    layout: Layout::Splits,
                    active: 0,
                    tree: "0".into(),
                    panes: vec![SavedPane::default()],
                },
            ],
        };
        let text = toml::to_string(&session).unwrap();
        assert_eq!(toml::from_str::<Session>(&text).unwrap(), session);
    }
}
