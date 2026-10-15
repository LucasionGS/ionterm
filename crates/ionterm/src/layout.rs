//! Arranging a tab's panes: kitty-style layouts over a split tree.
//!
//! The split tree is kept for every tab, so switching to another layout and
//! back restores the splits exactly. The other layouts ignore it and arrange
//! panes by their order in the tab.

use serde::{Deserialize, Serialize};

pub type PaneId = u64;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { x, y, width, height }
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    fn center(&self) -> (f32, f32) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    /// Splits into two along `direction` at `ratio` (0–1, share of the first).
    fn split(&self, direction: Direction, ratio: f32) -> (Rect, Rect) {
        match direction {
            Direction::Horizontal => {
                let w = (self.width * ratio).round();
                (
                    Rect::new(self.x, self.y, w, self.height),
                    Rect::new(self.x + w, self.y, self.width - w, self.height),
                )
            }
            Direction::Vertical => {
                let h = (self.height * ratio).round();
                (
                    Rect::new(self.x, self.y, self.width, h),
                    Rect::new(self.x, self.y + h, self.width, self.height - h),
                )
            }
        }
    }
}

/// How a split divides its space: side by side, or one above the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// Free splits in any direction.
    #[default]
    Splits,
    /// The first pane on the left, the rest stacked on the right.
    Tall,
    /// The first pane on top, the rest side by side below.
    Fat,
    Grid,
    /// Only the active pane, filling the tab.
    Stack,
}

/// The line between the two halves of a split, for dragging it.
#[derive(Clone, Debug, PartialEq)]
pub struct SplitBorder {
    /// The way to the split from the root: `false` = first child.
    pub path: Vec<bool>,
    /// `Horizontal` splits side by side, so their border is upright.
    pub direction: Direction,
    /// The split's whole area.
    pub area: Rect,
    /// Where the border is: an x for side-by-side splits, a y otherwise.
    pub at: f32,
}

impl SplitBorder {
    /// The split's ratio with the border moved to `pos` (an x or a y).
    pub fn ratio_at(&self, pos: f32) -> f32 {
        let (start, size) = match self.direction {
            Direction::Horizontal => (self.area.x, self.area.width),
            Direction::Vertical => (self.area.y, self.area.height),
        };
        ((pos - start) / size.max(1.0)).clamp(0.1, 0.9)
    }

    /// Whether a point is within `slop` pixels of the border.
    pub fn near(&self, x: f32, y: f32, slop: f32) -> bool {
        let a = self.area;
        match self.direction {
            Direction::Horizontal => (x - self.at).abs() <= slop && y >= a.y && y <= a.y + a.height,
            Direction::Vertical => (y - self.at).abs() <= slop && x >= a.x && x <= a.x + a.width,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SplitNode {
    Leaf(PaneId),
    Split {
        direction: Direction,
        ratio: f32,
        first: Box<SplitNode>,
        second: Box<SplitNode>,
    },
}

impl SplitNode {
    /// Replaces the leaf `target` with a split holding it and `new`.
    pub fn split(&mut self, target: PaneId, new: PaneId, direction: Direction) -> bool {
        match self {
            SplitNode::Leaf(id) if *id == target => {
                *self = SplitNode::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(SplitNode::Leaf(target)),
                    second: Box::new(SplitNode::Leaf(new)),
                };
                true
            }
            SplitNode::Leaf(_) => false,
            SplitNode::Split { first, second, .. } => {
                first.split(target, new, direction) || second.split(target, new, direction)
            }
        }
    }

    /// Removes a leaf; its sibling takes the parent's place. Returns `None`
    /// when the tree held only that leaf.
    pub fn remove(self, target: PaneId) -> Option<SplitNode> {
        match self {
            SplitNode::Leaf(id) if id == target => None,
            SplitNode::Leaf(_) => Some(self),
            SplitNode::Split {
                direction,
                ratio,
                first,
                second,
            } => match (first.remove(target), second.remove(target)) {
                (Some(first), Some(second)) => Some(SplitNode::Split {
                    direction,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    /// Exchanges two leaves' positions.
    pub fn swap(&mut self, a: PaneId, b: PaneId) {
        match self {
            SplitNode::Leaf(id) if *id == a => *id = b,
            SplitNode::Leaf(id) if *id == b => *id = a,
            SplitNode::Leaf(_) => {}
            SplitNode::Split { first, second, .. } => {
                first.swap(a, b);
                second.swap(a, b);
            }
        }
    }

    /// Moves the boundary nearest to `target` in `direction` by `delta` of
    /// the split's size. Returns whether a split was found.
    pub fn resize(&mut self, target: PaneId, direction: Direction, delta: f32) -> bool {
        self.resize_inner(target, direction, delta).unwrap_or(false)
    }

    /// `None` = target not in this subtree; `Some(done)` otherwise.
    fn resize_inner(&mut self, target: PaneId, dir: Direction, delta: f32) -> Option<bool> {
        match self {
            SplitNode::Leaf(id) => (*id == target).then_some(false),
            SplitNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let (found, in_first) = match first.resize_inner(target, dir, delta) {
                    Some(true) => return Some(true),
                    Some(false) => (true, true),
                    None => match second.resize_inner(target, dir, delta) {
                        Some(true) => return Some(true),
                        Some(false) => (true, false),
                        None => (false, false),
                    },
                };
                if !found {
                    return None;
                }
                if *direction == dir {
                    // Growing the first child moves the boundary toward the second.
                    let change = if in_first { delta } else { -delta };
                    *ratio = (*ratio + change).clamp(0.1, 0.9);
                    return Some(true);
                }
                Some(false)
            }
        }
    }

    /// Every split's border within `area`.
    pub fn borders(&self, area: Rect) -> Vec<SplitBorder> {
        let mut out = Vec::new();
        self.collect_borders(area, &mut Vec::new(), &mut out);
        out
    }

    fn collect_borders(&self, area: Rect, path: &mut Vec<bool>, out: &mut Vec<SplitBorder>) {
        let SplitNode::Split {
            direction,
            ratio,
            first,
            second,
        } = self
        else {
            return;
        };
        let (a, b) = area.split(*direction, *ratio);
        let at = match direction {
            Direction::Horizontal => b.x,
            Direction::Vertical => b.y,
        };
        out.push(SplitBorder {
            path: path.clone(),
            direction: *direction,
            area,
            at,
        });
        path.push(false);
        first.collect_borders(a, path, out);
        path.pop();
        path.push(true);
        second.collect_borders(b, path, out);
        path.pop();
    }

    /// Sets the ratio of the split at `path`.
    pub fn set_ratio(&mut self, path: &[bool], value: f32) {
        let SplitNode::Split { ratio, first, second, .. } = self else { return };
        match path.split_first() {
            None => *ratio = value.clamp(0.1, 0.9),
            Some((false, rest)) => first.set_ratio(rest, value),
            Some((true, rest)) => second.set_ratio(rest, value),
        }
    }

    fn arrange(&self, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            SplitNode::Leaf(id) => out.push((*id, area)),
            SplitNode::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let (a, b) = area.split(*direction, *ratio);
                first.arrange(a, out);
                second.arrange(b, out);
            }
        }
    }
}

/// Places every pane. `order` is the tab's pane order. The stack layout gives
/// every pane the full area; the caller shows only the active one.
pub fn arrange(layout: Layout, tree: &SplitNode, order: &[PaneId], area: Rect) -> Vec<(PaneId, Rect)> {
    let n = order.len();
    let mut out = Vec::with_capacity(n);
    if n <= 1 || layout == Layout::Stack {
        out.extend(order.iter().map(|&id| (id, area)));
        return out;
    }
    match layout {
        Layout::Splits => tree.arrange(area, &mut out),
        Layout::Tall | Layout::Fat => {
            let (main_dir, rest_dir) = if layout == Layout::Tall {
                (Direction::Horizontal, Direction::Vertical)
            } else {
                (Direction::Vertical, Direction::Horizontal)
            };
            let (main, rest) = area.split(main_dir, 0.5);
            out.push((order[0], main));
            out.extend(order[1..].iter().copied().zip(even_split(rest, rest_dir, n - 1)));
        }
        Layout::Grid => {
            let cols = (n as f32).sqrt().ceil() as usize;
            let rows = n.div_ceil(cols);
            let mut ids = order.iter();
            for (row, row_rect) in even_split(area, Direction::Vertical, rows).into_iter().enumerate() {
                // The last row takes whatever is left, stretched across.
                let in_row = if row == rows - 1 { n - cols * (rows - 1) } else { cols };
                for rect in even_split(row_rect, Direction::Horizontal, in_row) {
                    if let Some(&id) = ids.next() {
                        out.push((id, rect));
                    }
                }
            }
        }
        Layout::Stack => unreachable!("handled above"),
    }
    out
}

fn even_split(area: Rect, direction: Direction, count: usize) -> Vec<Rect> {
    let mut rects = Vec::with_capacity(count);
    let mut rest = area;
    for remaining in (1..=count).rev() {
        if remaining == 1 {
            rects.push(rest);
        } else {
            let (a, b) = rest.split(direction, 1.0 / remaining as f32);
            rects.push(a);
            rest = b;
        }
    }
    rects
}

/// The pane whose rectangle lies next to `from` in a direction: `dx`/`dy` are
/// -1, 0 or 1. Picks the closest one that overlaps on the other axis.
pub fn neighbor(rects: &[(PaneId, Rect)], from: PaneId, dx: i32, dy: i32) -> Option<PaneId> {
    let (_, origin) = rects.iter().find(|(id, _)| *id == from)?;
    let (cx, cy) = origin.center();
    rects
        .iter()
        .filter(|(id, _)| *id != from)
        .filter(|(_, r)| match (dx, dy) {
            (1, _) => r.x >= origin.x + origin.width - 1.0 && overlaps(r.y, r.height, origin.y, origin.height),
            (-1, _) => r.x + r.width <= origin.x + 1.0 && overlaps(r.y, r.height, origin.y, origin.height),
            (_, 1) => r.y >= origin.y + origin.height - 1.0 && overlaps(r.x, r.width, origin.x, origin.width),
            _ => r.y + r.height <= origin.y + 1.0 && overlaps(r.x, r.width, origin.x, origin.width),
        })
        .min_by(|(_, a), (_, b)| {
            let distance = |r: &Rect| {
                let (x, y) = r.center();
                (x - cx).abs() + (y - cy).abs()
            };
            distance(a).total_cmp(&distance(b))
        })
        .map(|(id, _)| *id)
}

fn overlaps(a: f32, a_len: f32, b: f32, b_len: f32) -> bool {
    a < b + b_len && b < a + a_len
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect {
        x: 0.0,
        y: 0.0,
        width: 100.0,
        height: 60.0,
    };

    #[test]
    fn splits_and_removes() {
        let mut tree = SplitNode::Leaf(1);
        assert!(tree.split(1, 2, Direction::Horizontal));
        assert!(tree.split(2, 3, Direction::Vertical));
        let rects = arrange(Layout::Splits, &tree, &[1, 2, 3], AREA);
        assert_eq!(rects[0], (1, Rect::new(0.0, 0.0, 50.0, 60.0)));
        assert_eq!(rects[1], (2, Rect::new(50.0, 0.0, 50.0, 30.0)));
        assert_eq!(rects[2], (3, Rect::new(50.0, 30.0, 50.0, 30.0)));

        let tree = tree.remove(2).unwrap();
        let rects = arrange(Layout::Splits, &tree, &[1, 3], AREA);
        assert_eq!(rects[1], (3, Rect::new(50.0, 0.0, 50.0, 60.0)));
        assert_eq!(tree.remove(1).unwrap().remove(3), None);
    }

    #[test]
    fn tall_grid_and_stack() {
        let tree = SplitNode::Leaf(1);
        let tall = arrange(Layout::Tall, &tree, &[1, 2, 3], AREA);
        assert_eq!(tall[0].1, Rect::new(0.0, 0.0, 50.0, 60.0));
        assert_eq!(tall[2].1, Rect::new(50.0, 30.0, 50.0, 30.0));

        let grid = arrange(Layout::Grid, &tree, &[1, 2, 3], AREA);
        assert_eq!(grid[0].1, Rect::new(0.0, 0.0, 50.0, 30.0));
        assert_eq!(grid[2].1, Rect::new(0.0, 30.0, 100.0, 30.0));

        let stack = arrange(Layout::Stack, &tree, &[1, 2], AREA);
        assert!(stack.iter().all(|(_, r)| *r == AREA));
    }

    #[test]
    fn finds_neighbors_and_resizes() {
        let mut tree = SplitNode::Leaf(1);
        tree.split(1, 2, Direction::Horizontal);
        tree.split(2, 3, Direction::Vertical);
        let rects = arrange(Layout::Splits, &tree, &[1, 2, 3], AREA);
        assert_eq!(neighbor(&rects, 1, 1, 0), Some(2));
        assert_eq!(neighbor(&rects, 3, 0, -1), Some(2));
        assert_eq!(neighbor(&rects, 3, -1, 0), Some(1));
        assert_eq!(neighbor(&rects, 1, -1, 0), None);

        assert!(tree.resize(1, Direction::Horizontal, 0.1));
        let rects = arrange(Layout::Splits, &tree, &[1, 2, 3], AREA);
        assert_eq!(rects[0].1.width, 60.0);
    }

    #[test]
    fn drags_borders() {
        let mut tree = SplitNode::Leaf(1);
        tree.split(1, 2, Direction::Horizontal);
        tree.split(2, 3, Direction::Vertical);
        let borders = tree.borders(AREA);
        assert_eq!(borders.len(), 2);
        assert!(borders[0].near(52.0, 10.0, 3.0) && !borders[0].near(56.0, 10.0, 3.0));
        assert_eq!(borders[1].path, vec![true]);
        assert!(borders[1].near(70.0, 31.0, 3.0) && !borders[1].near(20.0, 31.0, 3.0));

        tree.set_ratio(&borders[0].path, borders[0].ratio_at(25.0));
        tree.set_ratio(&borders[1].path, borders[1].ratio_at(59.0));
        let rects = arrange(Layout::Splits, &tree, &[1, 2, 3], AREA);
        assert_eq!(rects[0].1.width, 25.0);
        // Dragging stops short of squashing a pane.
        assert_eq!(rects[2].1.height, 6.0);
    }
}
