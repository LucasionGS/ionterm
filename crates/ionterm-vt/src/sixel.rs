//! Streaming Sixel decoder.
//!
//! Sixel data arrives in a DCS sequence (`ESC P <params> q <data> ESC \`).
//! Each data byte paints a column of six vertical pixels in the current
//! colour; `$` returns to the left edge and `-` moves down six pixels. The
//! decoder paints straight into an RGBA canvas that grows as needed, so even
//! large images are never buffered as text.

/// Largest width or height accepted, in pixels.
const MAX_DIMENSION: usize = 10_000;
const REGISTERS: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Data,
    /// `!` repeat count being read.
    Repeat,
    /// `#` colour parameters being read.
    Color,
    /// `"` raster attributes being read.
    Raster,
}

pub struct SixelDecoder {
    state: State,
    params: Vec<u32>,
    palette: Vec<[u8; 3]>,
    color: usize,
    x: usize,
    /// Top of the current six-pixel band.
    y: usize,
    width: usize,
    height: usize,
    /// RGBA; alpha 0 marks pixels never painted.
    pixels: Vec<u8>,
    /// The painted extent, which is the image's size.
    used_width: usize,
    used_height: usize,
    /// Pixels never painted are filled with this colour, or left transparent.
    background: Option<[u8; 3]>,
}

impl SixelDecoder {
    /// `params` are the DCS parameters; `background` is the terminal's
    /// background, used when the image asks for an opaque background.
    pub fn new(params: &[u16], background: [u8; 3]) -> Self {
        // P2 = 1 leaves unpainted pixels transparent; 0 and 2 fill them.
        let transparent = params.get(1).copied() == Some(1);
        Self {
            state: State::Data,
            params: Vec::new(),
            palette: default_palette(),
            color: 0,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            pixels: Vec::new(),
            used_width: 0,
            used_height: 0,
            background: (!transparent).then_some(background),
        }
    }

    pub fn put(&mut self, byte: u8) {
        if self.state != State::Data {
            match byte {
                b'0'..=b'9' => {
                    let last = self.params.last_mut();
                    match last {
                        Some(value) => *value = value.saturating_mul(10).saturating_add(u32::from(byte - b'0')),
                        None => self.params.push(u32::from(byte - b'0')),
                    }
                    return;
                }
                b';' => {
                    if self.params.is_empty() {
                        self.params.push(0);
                    }
                    self.params.push(0);
                    return;
                }
                _ => {
                    let repeat = self.state == State::Repeat;
                    self.finish_params(byte);
                    // A repeat's byte is the sixel it repeats: already painted.
                    if repeat {
                        return;
                    }
                }
            }
        }
        match byte {
            b'?'..=b'~' if self.state == State::Data => self.paint(byte - b'?', 1),
            b'!' => self.begin(State::Repeat),
            b'#' => self.begin(State::Color),
            b'"' => self.begin(State::Raster),
            b'$' => self.x = 0,
            b'-' => {
                self.x = 0;
                self.y += 6;
            }
            _ => {}
        }
    }

    fn begin(&mut self, state: State) {
        self.state = state;
        self.params.clear();
    }

    /// Ends a parameter run at a non-digit byte. For a repeat, that byte is
    /// the sixel to repeat, and it is painted here.
    fn finish_params(&mut self, byte: u8) {
        let params = std::mem::take(&mut self.params);
        let state = std::mem::replace(&mut self.state, State::Data);
        match state {
            State::Repeat => {
                if let b'?'..=b'~' = byte {
                    let count = params.first().copied().unwrap_or(1).max(1) as usize;
                    self.paint(byte - b'?', count);
                }
            }
            State::Color => {
                let register = params.first().copied().unwrap_or(0) as usize % REGISTERS;
                if params.len() >= 5 {
                    let (a, b, c) = (params[2], params[3], params[4]);
                    self.palette[register] = match params[1] {
                        1 => hls_to_rgb(a, b, c),
                        _ => [percent(a), percent(b), percent(c)],
                    };
                }
                self.color = register;
            }
            State::Raster => {
                // "Pan;Pad;Ph;Pv: preallocate the stated size.
                if let (Some(&w), Some(&h)) = (params.get(2), params.get(3)) {
                    self.ensure(w as usize, h as usize);
                }
            }
            State::Data => {}
        }
    }

    /// Paints `count` columns of a six-pixel pattern at the cursor.
    fn paint(&mut self, bits: u8, count: usize) {
        let count = count.min(MAX_DIMENSION.saturating_sub(self.x));
        if count == 0 || self.y + 6 > MAX_DIMENSION {
            return;
        }
        if bits != 0 {
            self.ensure(self.x + count, self.y + 6);
            let [r, g, b] = self.palette[self.color];
            for bit in 0..6 {
                if bits & (1 << bit) == 0 {
                    continue;
                }
                let row = self.y + bit;
                let start = (row * self.width + self.x) * 4;
                for px in self.pixels[start..start + count * 4].chunks_exact_mut(4) {
                    px.copy_from_slice(&[r, g, b, 255]);
                }
                self.used_height = self.used_height.max(row + 1);
            }
        }
        self.x += count;
        self.used_width = self.used_width.max(self.x);
    }

    /// Grows the canvas to at least `width` x `height`.
    fn ensure(&mut self, width: usize, height: usize) {
        let width = width.min(MAX_DIMENSION);
        let height = height.min(MAX_DIMENSION);
        if width <= self.width && height <= self.height {
            return;
        }
        // Grow generously so long images don't re-copy every band.
        let new_width = width.max(self.width);
        let new_height = height.max(self.height).max(self.height * 3 / 2).min(MAX_DIMENSION);
        let mut pixels = vec![0u8; new_width * new_height * 4];
        for row in 0..self.height {
            let old = row * self.width * 4;
            let new = row * new_width * 4;
            pixels[new..new + self.width * 4].copy_from_slice(&self.pixels[old..old + self.width * 4]);
        }
        self.pixels = pixels;
        self.width = new_width;
        self.height = new_height;
    }

    /// The finished image as (width, height, RGBA), or `None` if nothing
    /// was painted.
    pub fn finish(self) -> Option<(u32, u32, Vec<u8>)> {
        let (w, h) = (self.used_width, self.used_height);
        if w == 0 || h == 0 {
            return None;
        }
        let mut out = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            let start = row * self.width * 4;
            out.extend_from_slice(&self.pixels[start..start + w * 4]);
        }
        if let Some([r, g, b]) = self.background {
            for px in out.chunks_exact_mut(4).filter(|px| px[3] == 0) {
                px.copy_from_slice(&[r, g, b, 255]);
            }
        }
        Some((w as u32, h as u32, out))
    }
}

fn percent(value: u32) -> u8 {
    (value.min(100) * 255 / 100) as u8
}

/// Sixel HLS: hue in degrees with blue at 0, red at 120 and green at 240;
/// lightness and saturation in percent.
fn hls_to_rgb(hue: u32, lightness: u32, saturation: u32) -> [u8; 3] {
    let h = ((hue + 240) % 360) as f32 / 360.0;
    let l = lightness.min(100) as f32 / 100.0;
    let s = saturation.min(100) as f32 / 100.0;
    if s == 0.0 {
        let v = (l * 255.0).round() as u8;
        return [v, v, v];
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let channel = |t: f32| {
        let t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v * 255.0).round() as u8
    };
    [channel(h + 1.0 / 3.0), channel(h), channel(h - 1.0 / 3.0)]
}

/// The VT340's default 16 colours; the remaining registers start black.
fn default_palette() -> Vec<[u8; 3]> {
    let vt340: [[u32; 3]; 16] = [
        [0, 0, 0],
        [20, 20, 80],
        [80, 13, 13],
        [20, 80, 20],
        [80, 20, 80],
        [20, 80, 80],
        [80, 80, 20],
        [53, 53, 53],
        [26, 26, 26],
        [33, 33, 60],
        [60, 26, 26],
        [33, 60, 33],
        [60, 33, 60],
        [33, 60, 60],
        [60, 60, 33],
        [80, 80, 80],
    ];
    let mut palette = vec![[0, 0, 0]; REGISTERS];
    for (slot, rgb) in palette.iter_mut().zip(vt340) {
        *slot = rgb.map(percent);
    }
    palette
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(data: &[u8], params: &[u16]) -> (u32, u32, Vec<u8>) {
        let mut decoder = SixelDecoder::new(params, [1, 2, 3]);
        for &b in data {
            decoder.put(b);
        }
        decoder.finish().unwrap()
    }

    #[test]
    fn paints_columns_and_repeats() {
        // Red, 3 columns of all six pixels via repeat, then one more column.
        let (w, h, rgba) = decode(b"#1;2;100;0;0!3~~", &[0, 1]);
        assert_eq!((w, h), (4, 6));
        assert_eq!(&rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(&rgba[(5 * 4 + 3) * 4..(5 * 4 + 3) * 4 + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn new_lines_and_backgrounds() {
        // One pixel (bit 0) on the first band, then the next band.
        let (w, h, rgba) = decode(b"#2;2;0;100;0@-@", &[0, 0]);
        assert_eq!((w, h), (1, 7));
        // Row 1 was never painted: filled with the background colour.
        assert_eq!(&rgba[4..8], &[1, 2, 3, 255]);
        assert_eq!(&rgba[6 * 4..6 * 4 + 4], &[0, 255, 0, 255]);
    }

    #[test]
    fn hls_hues_follow_dec() {
        assert_eq!(hls_to_rgb(0, 50, 100), [0, 0, 255]);
        assert_eq!(hls_to_rgb(120, 50, 100), [255, 0, 0]);
        assert_eq!(hls_to_rgb(240, 50, 100), [0, 255, 0]);
    }
}
