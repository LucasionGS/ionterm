//! The cursor trail (`[cursor] trail`): when the cursor jumps, a fading
//! streak runs from where it was and shrinks into where it went, so the eye
//! can follow it.

use std::time::{Duration, Instant};

use ionterm_render::ChromeItem;
use ionterm_vt::Rgb;

/// How long the streak takes to catch up with the cursor.
const DURATION: Duration = Duration::from_millis(160);
/// Moves of at most this many cells (typing, arrow keys) leave no trail.
const THRESHOLD_CELLS: f32 = 2.0;
/// Quads the streak is drawn with, at least: long jumps get more, so they
/// overlap into one streak instead of dashes.
const STEPS: usize = 12;
const MAX_STEPS: usize = 160;
/// How thin the streak gets at its tail, as a share of the cursor.
const TAIL_SIZE: f32 = 0.35;

#[derive(Default)]
pub struct Trail {
    /// The tab the cursor was followed in; switching tabs starts over.
    tab: Option<u64>,
    /// Where the cursor was last frame (x, y, width, height).
    last: Option<[f32; 4]>,
    from: [f32; 4],
    to: [f32; 4],
    started: Option<Instant>,
}

impl Trail {
    /// Follows the cursor (`None` while it's hidden or elsewhere); a long
    /// enough jump starts the streak.
    pub fn update(&mut self, tab: u64, cursor: Option<[f32; 4]>, now: Instant) {
        if self.tab != Some(tab) {
            *self = Self {
                tab: Some(tab),
                last: cursor,
                ..Self::default()
            };
            return;
        }
        let Some(to) = cursor else {
            self.last = None;
            return;
        };
        if let Some(from) = self.last
            && from != to
        {
            let (w, h) = (to[2].max(1.0), to[3].max(1.0));
            let cells = ((to[0] - from[0]).abs() / w).max((to[1] - from[1]).abs() / h);
            if cells > THRESHOLD_CELLS {
                // A jump during a jump starts from where the streak's tail is.
                self.from = self.tail(now).unwrap_or(from);
                self.started = Some(now);
            }
            self.to = to;
        }
        self.last = Some(to);
    }

    /// How far along the streak is, 0–1, while it runs.
    fn progress(&self, now: Instant) -> Option<f32> {
        let t = now.duration_since(self.started?).as_secs_f32() / DURATION.as_secs_f32();
        (t < 1.0).then_some(t)
    }

    /// The streak's far end: it starts where the cursor was and eases in.
    fn tail(&self, now: Instant) -> Option<[f32; 4]> {
        let t = self.progress(now)?;
        Some(lerp(self.from, self.to, 1.0 - (1.0 - t).powi(3)))
    }

    /// Draws the streak. Returns whether it's still moving.
    pub fn draw(&self, now: Instant, color: Rgb, out: &mut Vec<ChromeItem>) -> bool {
        let (Some(t), Some(tail)) = (self.progress(now), self.tail(now)) else { return false };
        let distance = (tail[0] - self.to[0]).hypot(tail[1] - self.to[1]);
        let thinnest = (self.to[2].min(self.to[3]) * TAIL_SIZE).max(1.0);
        let steps = ((distance / (thinnest / 2.0)).ceil() as usize).clamp(STEPS, MAX_STEPS);
        let spacing = (distance / steps as f32).max(0.5);
        for step in 0..steps {
            // 0 at the tail, 1 at the cursor: narrower and fainter backwards.
            let f = step as f32 / steps as f32;
            let [x, y, w, h] = lerp(tail, self.to, f);
            let size = TAIL_SIZE + (1.0 - TAIL_SIZE) * f;
            let (sw, sh) = (w * size, h * size);
            // Overlapping quads add up: share the opacity between them.
            let overlap = (sw.min(sh) / spacing).max(1.0);
            out.push(ChromeItem::RoundedRect {
                rect: [x + (w - sw) / 2.0, y + (h - sh) / 2.0, sw, sh],
                radius: (sw.min(sh) / 2.0).floor(),
                top_only: false,
                color,
                alpha: (1.0 - t) * 0.6 * f.powf(1.5) / overlap,
            });
        }
        true
    }
}

fn lerp(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: [f32; 4] = [0.0, 0.0, 10.0, 20.0];

    fn at(col: f32, row: f32) -> Option<[f32; 4]> {
        Some([col * 10.0, row * 20.0, 10.0, 20.0])
    }

    #[test]
    fn jumps_leave_a_trail_and_typing_doesnt() {
        let start = Instant::now();
        let mut trail = Trail::default();
        trail.update(1, at(0.0, 0.0), start);
        trail.update(1, at(1.0, 0.0), start);
        assert!(trail.progress(start).is_none());
        trail.update(1, at(1.0, 5.0), start);
        assert!(trail.progress(start).is_some());
        assert_eq!(trail.tail(start), at(1.0, 0.0));
        let mut out = Vec::new();
        assert!(trail.draw(start + Duration::from_millis(50), Rgb::default(), &mut out));
        assert!(out.len() >= STEPS && out.len() <= MAX_STEPS);
        assert!(!trail.draw(start + DURATION, Rgb::default(), &mut out));
        // Another tab, or a hidden cursor, starts over.
        trail.update(2, at(30.0, 9.0), start);
        assert!(trail.progress(start).is_none());
        trail.update(2, None, start);
        trail.update(2, Some(CELL), start);
        assert!(trail.progress(start).is_none());
    }
}
