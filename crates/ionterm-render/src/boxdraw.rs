//! Box-drawing, block-element and Powerline glyphs drawn by us rather than the
//! font, so they fill the cell exactly and join seamlessly with neighbours no
//! matter the font's metrics or the line height.

/// Line weights for one arm of a box-drawing character.
const NONE: u8 = 0;
const LIGHT: u8 = 1;
const HEAVY: u8 = 2;
const DOUBLE: u8 = 3;

/// Arms (up, right, down, left) for U+2500..=U+257F as four digits each.
/// `A` marks arcs and `D` diagonals, which are drawn separately.
const BOX_TABLE: [&str; 128] = [
    "0101", "0202", "1010", "2020", "0101", "0202", "1010", "2020", // 2500
    "0101", "0202", "1010", "2020", "0110", "0210", "0120", "0220", // 2508
    "0011", "0012", "0021", "0022", "1100", "1200", "2100", "2200", // 2510
    "1001", "1002", "2001", "2002", "1110", "1210", "2110", "1120", // 2518
    "2120", "2210", "1220", "2220", "1011", "1012", "2011", "1021", // 2520
    "2021", "2012", "1022", "2022", "0111", "0112", "0211", "0212", // 2528
    "0121", "0122", "0221", "0222", "1101", "1102", "1201", "1202", // 2530
    "2101", "2102", "2201", "2202", "1111", "1112", "1211", "1212", // 2538
    "2111", "1121", "2121", "2112", "2211", "1122", "1221", "2212", // 2540
    "1222", "2122", "2221", "2222", "0101", "0202", "1010", "2020", // 2548
    "0303", "3030", "0310", "0130", "0330", "0013", "0031", "0033", // 2550
    "1300", "3100", "3300", "1003", "3001", "3003", "1310", "3130", // 2558
    "3330", "1013", "3031", "3033", "0313", "0131", "0333", "1303", // 2560
    "3101", "3303", "1313", "3131", "3333", "A", "A", "A", // 2568
    "A", "D", "D", "D", "0001", "1000", "0100", "0010", // 2570
    "0002", "2000", "0200", "0020", "0201", "1020", "0102", "2010", // 2578
];

pub fn is_procedural(c: char) -> bool {
    matches!(c as u32, 0x2500..=0x257F | 0x2580..=0x259F | 0xE0B0..=0xE0B4 | 0xE0B6)
}

/// An alpha mask of `width * height` bytes, or `None` if `c` isn't drawn here.
pub fn render(c: char, width: u32, height: u32, light: u32) -> Option<Vec<u8>> {
    let mut canvas = Canvas::new(width as usize, height as usize);
    let cp = c as u32;
    match cp {
        0x2500..=0x257F => {
            let spec = BOX_TABLE[(cp - 0x2500) as usize];
            match spec {
                "A" => canvas.arc(cp, light as f32),
                "D" => canvas.diagonal(cp, light as f32),
                _ => {
                    let w: Vec<u8> = spec.bytes().map(|b| b - b'0').collect();
                    canvas.lines([w[0], w[1], w[2], w[3]], light as usize);
                }
            }
        }
        0x2580..=0x259F => canvas.block(cp),
        0xE0B0..=0xE0B4 | 0xE0B6 => canvas.powerline(cp, light as f32),
        _ => return None,
    }
    Some(canvas.pixels)
}

struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; width * height],
        }
    }

    fn rect(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, alpha: u8) {
        for y in y0.min(self.height)..y1.min(self.height) {
            for x in x0.min(self.width)..x1.min(self.width) {
                let p = &mut self.pixels[y * self.width + x];
                *p = (*p).max(alpha);
            }
        }
    }

    /// Fills pixels by coverage, supersampled 4x4 for anti-aliasing.
    fn fill(&mut self, inside: impl Fn(f32, f32) -> bool) {
        for y in 0..self.height {
            for x in 0..self.width {
                let mut hits = 0u32;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let px = x as f32 + (sx as f32 + 0.5) / 4.0;
                        let py = y as f32 + (sy as f32 + 0.5) / 4.0;
                        hits += u32::from(inside(px, py));
                    }
                }
                let p = &mut self.pixels[y * self.width + x];
                *p = (*p).max((hits * 255 / 16) as u8);
            }
        }
    }

    /// Straight arms from the centre to each edge. Arms reach past the centre
    /// by half the perpendicular arm's thickness so junctions are solid.
    fn lines(&mut self, [up, right, down, left]: [u8; 4], light: usize) {
        let thickness = |weight: u8| match weight {
            LIGHT | DOUBLE => light,
            HEAVY => light * 2,
            _ => 0,
        };
        let (cx, cy) = (self.width / 2, self.height / 2);
        let gap = light;
        let v_th = thickness(up).max(thickness(down)).max(light);
        let h_th = thickness(left).max(thickness(right)).max(light);
        let any_double = [up, right, down, left].contains(&DOUBLE);

        // Horizontal arms.
        for (weight, is_left) in [(left, true), (right, false)] {
            if weight == NONE {
                continue;
            }
            if weight == DOUBLE {
                // Two lines at cy ± gap; a line stops short of the centre when
                // a perpendicular arm sits on its side (it's the inner line).
                for (line_y, side_arm) in [(cy - gap.min(cy), up), (cy + gap, down)] {
                    let inner = side_arm != NONE;
                    let (from, to) = if is_left {
                        (0, if inner { cx - gap.min(cx) } else { cx + gap } + light / 2 + 1)
                    } else {
                        ((if inner { cx + gap } else { cx - gap.min(cx) }).saturating_sub(light / 2), self.width)
                    };
                    self.rect(from, line_y.saturating_sub(light / 2), to, line_y.saturating_sub(light / 2) + light, 255);
                }
                continue;
            }
            let th = thickness(weight);
            let reach = if any_double { gap + light } else { v_th / 2 + v_th % 2 };
            let (from, to) = if is_left { (0, cx + reach) } else { (cx.saturating_sub(reach), self.width) };
            self.rect(from, cy.saturating_sub(th / 2), to, cy.saturating_sub(th / 2) + th, 255);
        }

        // Vertical arms.
        for (weight, is_up) in [(up, true), (down, false)] {
            if weight == NONE {
                continue;
            }
            if weight == DOUBLE {
                for (line_x, side_arm) in [(cx - gap.min(cx), left), (cx + gap, right)] {
                    let inner = side_arm != NONE;
                    let (from, to) = if is_up {
                        (0, if inner { cy - gap.min(cy) } else { cy + gap } + light / 2 + 1)
                    } else {
                        ((if inner { cy + gap } else { cy - gap.min(cy) }).saturating_sub(light / 2), self.height)
                    };
                    self.rect(line_x.saturating_sub(light / 2), from, line_x.saturating_sub(light / 2) + light, to, 255);
                }
                continue;
            }
            let th = thickness(weight);
            let reach = if any_double { gap + light } else { h_th / 2 + h_th % 2 };
            let (from, to) = if is_up { (0, cy + reach) } else { (cy.saturating_sub(reach), self.height) };
            self.rect(cx.saturating_sub(th / 2), from, cx.saturating_sub(th / 2) + th, to, 255);
        }
    }

    /// Rounded corners ╭╮╯╰: a quarter circle plus straight runs to the edges.
    fn arc(&mut self, cp: u32, light: f32) {
        let (w, h) = (self.width as f32, self.height as f32);
        let (cx, cy) = ((self.width / 2) as f32 + 0.5, (self.height / 2) as f32 + 0.5);
        let r = (w / 2.0).min(h / 2.0);
        // Which edges the arc connects: (horizontal direction, vertical direction).
        let (dx, dy) = match cp {
            0x256D => (1.0, 1.0),   // ╭ right + down
            0x256E => (-1.0, 1.0),  // ╮ left + down
            0x256F => (-1.0, -1.0), // ╯ left + up
            _ => (1.0, -1.0),       // ╰ right + up
        };
        let (acx, acy) = (cx + dx * r, cy + dy * r);
        let half = light / 2.0;
        self.fill(|x, y| {
            // Quarter circle in the quadrant facing the centre.
            let on_arc = (x - acx) * dx <= 0.0
                && (y - acy) * dy <= 0.0
                && (((x - acx).powi(2) + (y - acy).powi(2)).sqrt() - r).abs() <= half;
            let on_h = (y - cy).abs() <= half && (x - acx) * dx >= 0.0 && x >= 0.0 && x <= w;
            let on_v = (x - cx).abs() <= half && (y - acy) * dy >= 0.0 && y >= 0.0 && y <= h;
            on_arc || on_h || on_v
        });
    }

    fn diagonal(&mut self, cp: u32, light: f32) {
        let (w, h) = (self.width as f32, self.height as f32);
        let len = (w * w + h * h).sqrt();
        let half = light / 2.0 + 0.25;
        // Distance from (x, y) to the lines through opposite corners.
        let rising = move |x: f32, y: f32| ((h * x + w * y - w * h) / len).abs() <= half;
        let falling = move |x: f32, y: f32| ((h * x - w * y) / len).abs() <= half;
        match cp {
            0x2571 => self.fill(rising),
            0x2572 => self.fill(falling),
            _ => self.fill(|x, y| rising(x, y) || falling(x, y)),
        }
    }

    fn block(&mut self, cp: u32) {
        let (w, h) = (self.width, self.height);
        let eighth_h = |n: usize| (h * n + 4) / 8;
        let eighth_w = |n: usize| (w * n + 4) / 8;
        match cp {
            0x2580 => self.rect(0, 0, w, h / 2, 255),
            0x2581..=0x2588 => {
                let n = (cp - 0x2580) as usize;
                self.rect(0, h - eighth_h(n), w, h, 255);
            }
            0x2589..=0x258F => {
                let n = (0x2590 - cp) as usize;
                self.rect(0, 0, eighth_w(n), h, 255);
            }
            0x2590 => self.rect(w / 2, 0, w, h, 255),
            0x2591 => self.rect(0, 0, w, h, 64),
            0x2592 => self.rect(0, 0, w, h, 128),
            0x2593 => self.rect(0, 0, w, h, 192),
            0x2594 => self.rect(0, 0, w, eighth_h(1), 255),
            0x2595 => self.rect(w - eighth_w(1), 0, w, h, 255),
            0x2596..=0x259F => {
                // Quadrants as bits: upper-left, upper-right, lower-left, lower-right.
                const QUADS: [u8; 10] = [0b0010, 0b0001, 0b1000, 0b1011, 0b1001, 0b1110, 0b1101, 0b0100, 0b0110, 0b0111];
                let bits = QUADS[(cp - 0x2596) as usize];
                let (mx, my) = (w / 2, h / 2);
                if bits & 0b1000 != 0 {
                    self.rect(0, 0, mx, my, 255);
                }
                if bits & 0b0100 != 0 {
                    self.rect(mx, 0, w, my, 255);
                }
                if bits & 0b0010 != 0 {
                    self.rect(0, my, mx, h, 255);
                }
                if bits & 0b0001 != 0 {
                    self.rect(mx, my, w, h, 255);
                }
            }
            _ => {}
        }
    }

    fn powerline(&mut self, cp: u32, light: f32) {
        let (w, h) = (self.width as f32, self.height as f32);
        let mid = h / 2.0;
        // Horizontal distance from the flat edge to the triangle's slope at y.
        let reach = move |y: f32| w * (1.0 - ((y - mid).abs() / mid));
        match cp {
            0xE0B0 => self.fill(|x, y| x <= reach(y)),
            0xE0B2 => self.fill(|x, y| w - x <= reach(y)),
            0xE0B1 | 0xE0B3 => {
                let slope_len = (w * w + mid * mid).sqrt();
                let half = light / 2.0 + 0.25;
                let flip = cp == 0xE0B3;
                self.fill(|x, y| {
                    let x = if flip { w - x } else { x };
                    // Distance to the slope from the corner to the tip.
                    let (ty, sign) = if y < mid { (y, 1.0) } else { (h - y, 1.0) };
                    ((mid * x - w * ty) * sign / slope_len).abs() <= half
                });
            }
            0xE0B4 => self.fill(|x, y| (x / w).powi(2) + ((y - mid) / mid).powi(2) <= 1.0),
            0xE0B6 => self.fill(|x, y| ((w - x) / w).powi(2) + ((y - mid) / mid).powi(2) <= 1.0),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn horizontal_line_spans_the_cell() {
        let mask = render('─', 8, 16, 1).unwrap();
        let row = 8;
        assert!(mask[row * 8..row * 8 + 8].iter().all(|&a| a == 255));
        assert!(mask[..8].iter().all(|&a| a == 0));
    }

    #[test]
    fn full_block_is_solid() {
        assert!(render('█', 8, 16, 1).unwrap().iter().all(|&a| a == 255));
    }
}
