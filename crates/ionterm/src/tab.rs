//! A tab: its panes, their split tree, and the layout arranging them.

use ionterm_vt::Rgb;

use crate::files::panel::FilePanel;
use crate::settings::SettingsPage;
use crate::layout::{self, Direction, Layout, PaneId, Rect, SplitNode};
use crate::pane::Pane;

pub struct Tab {
    /// Stable identity across reordering (the id of the tab's first pane).
    pub id: u64,
    /// In creation order, which next/previous pane and the non-split
    /// layouts follow.
    pub panes: Vec<Pane>,
    tree: SplitNode,
    pub active: PaneId,
    pub layout: Layout,
    /// Temporarily shows only the active pane, without changing `layout`.
    pub zoomed: bool,
    /// Set by renaming; overrides the shell's title.
    pub custom_title: Option<String>,
    /// Set from the tab's menu.
    pub color: Option<Rgb>,
    /// The file panel docked on the right, while open.
    pub files: Option<FilePanel>,
    /// This is the settings tab: the page shows instead of the pane, which
    /// only runs the editor when a config file is opened.
    pub settings: Option<SettingsPage>,
    /// What happened here while another tab was shown.
    pub activity: crate::tabbar::Activity,
    /// Typing and pasting into the active pane goes to every pane here.
    pub broadcast: bool,
}

impl Tab {
    pub fn new(pane: Pane, layout: Layout) -> Self {
        Self {
            id: pane.id,
            tree: SplitNode::Leaf(pane.id),
            active: pane.id,
            panes: vec![pane],
            layout,
            zoomed: false,
            custom_title: None,
            color: None,
            files: None,
            settings: None,
            activity: crate::tabbar::Activity::None,
            broadcast: false,
        }
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    pub fn active_pane(&self) -> &Pane {
        self.pane(self.active).expect("a tab always has its active pane")
    }

    pub fn active_pane_mut(&mut self) -> &mut Pane {
        let id = self.active;
        self.pane_mut(id).expect("a tab always has its active pane")
    }

    fn effective_layout(&self) -> Layout {
        if self.zoomed { Layout::Stack } else { self.layout }
    }

    /// The panes that also get what's typed into `id`: the others in a
    /// broadcasting tab, except where input would do harm (a ZMODEM
    /// transfer) or go nowhere (a dropped connection).
    pub fn input_peers(&self, id: PaneId) -> impl Iterator<Item = &Pane> {
        let on = self.broadcast && id == self.active;
        self.panes
            .iter()
            .filter(move |p| on && p.id != id && p.disconnected.is_none() && !p.zmodem.is_active())
    }

    /// The borders that can be dragged: only the split layout has them.
    pub fn split_borders(&self, area: Rect) -> Vec<layout::SplitBorder> {
        if self.effective_layout() != Layout::Splits || self.visible_count() < 2 {
            return Vec::new();
        }
        self.tree.borders(self.pane_area(area))
    }

    pub fn set_split_ratio(&mut self, path: &[bool], ratio: f32) {
        self.tree.set_ratio(path, ratio);
    }

    /// The part of `area` left for panes beside the file panel.
    pub fn pane_area(&self, area: Rect) -> Rect {
        match &self.files {
            Some(panel) => Rect::new(area.x, area.y, (area.width - panel.rect.width).max(1.0), area.height),
            None => area,
        }
    }

    /// Where every pane goes within `area`.
    pub fn arrange(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let order: Vec<PaneId> = self.panes.iter().map(|p| p.id).collect();
        layout::arrange(self.effective_layout(), &self.tree, &order, area)
    }

    /// Whether a pane is drawn: in the stack layout only the active one is.
    pub fn is_visible(&self, id: PaneId) -> bool {
        self.effective_layout() != Layout::Stack || id == self.active
    }

    pub fn visible_count(&self) -> usize {
        if self.effective_layout() == Layout::Stack { 1 } else { self.panes.len() }
    }

    /// Adds a pane next to the active one and makes it active.
    pub fn add(&mut self, pane: Pane, direction: Direction) {
        let index = self.panes.iter().position(|p| p.id == self.active).map_or(self.panes.len(), |i| i + 1);
        self.tree.split(self.active, pane.id, direction);
        self.active = pane.id;
        self.panes.insert(index, pane);
        self.zoomed = false;
    }

    /// Removes a pane, activating the one before it. Returns the pane so the
    /// caller decides when it is dropped (dropping kills its shell).
    pub fn remove(&mut self, id: PaneId) -> Option<Pane> {
        let index = self.panes.iter().position(|p| p.id == id)?;
        let pane = self.panes.remove(index);
        let tree = std::mem::replace(&mut self.tree, SplitNode::Leaf(id));
        if let Some(tree) = tree.remove(id) {
            self.tree = tree;
        }
        if self.active == id
            && let Some(next) = self.panes.get(index.saturating_sub(1)).or(self.panes.first())
        {
            self.active = next.id;
        }
        Some(pane)
    }

    /// The pane `offset` steps away from the active one in creation order.
    pub fn cycle(&self, offset: isize) -> PaneId {
        let index = self.panes.iter().position(|p| p.id == self.active).unwrap_or(0) as isize;
        let next = (index + offset).rem_euclid(self.panes.len() as isize) as usize;
        self.panes[next].id
    }

    /// Swaps the active pane with its neighbour in creation order, moving it
    /// in the split tree too.
    pub fn move_active(&mut self, offset: isize) {
        let Some(index) = self.panes.iter().position(|p| p.id == self.active) else { return };
        let other = (index as isize + offset).rem_euclid(self.panes.len() as isize) as usize;
        if other != index {
            let other_id = self.panes[other].id;
            self.tree.swap(self.active, other_id);
            self.panes.swap(index, other);
        }
    }

    /// Grows (positive) or shrinks the active pane along a direction, in the
    /// split layout.
    pub fn resize_active(&mut self, direction: Direction, delta: f32) -> bool {
        self.effective_layout() == Layout::Splits && self.tree.resize(self.active, direction, delta)
    }
}
