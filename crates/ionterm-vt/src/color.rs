//! Colours as the terminal sees them: either a reference into the palette or a
//! direct RGB value. Resolving references to RGB is the renderer's job, since
//! it depends on the theme and on attributes like bold and inverse.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parses `#rrggbb` or `#rgb`.
    pub fn from_hex(s: &str) -> Option<Self> {
        let hex = s.strip_prefix('#')?;
        match hex.len() {
            6 => Some(Self::new(
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            )),
            3 => {
                let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|v| v * 17);
                Some(Self::new(digit(0)?, digit(1)?, digit(2)?))
            }
            _ => None,
        }
    }

    /// Parses the colour specs accepted by OSC 4/10/11/12: `#rgb`, `#rrggbb`
    /// and the X11 form `rgb:r/g/b` with 1–4 hex digits per channel.
    pub fn parse_spec(s: &str) -> Option<Self> {
        if s.starts_with('#') {
            return Self::from_hex(s);
        }
        let channels = s.strip_prefix("rgb:")?;
        let mut parts = channels.split('/').map(|part| {
            let value = u32::from_str_radix(part, 16).ok()?;
            let max = (1u32 << (4 * part.len().clamp(1, 4))) - 1;
            Some((value * 255 / max) as u8)
        });
        let rgb = Self::new(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(rgb)
    }

    /// The X11 `rgb:rrrr/gggg/bbbb` form used when answering colour queries.
    pub fn to_spec(self) -> String {
        format!(
            "rgb:{:04x}/{:04x}/{:04x}",
            u16::from(self.r) * 257,
            u16::from(self.g) * 257,
            u16::from(self.b) * 257
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Color {
    /// The default foreground or background, depending on where it is used.
    #[default]
    Default,
    Indexed(u8),
    Rgb(Rgb),
}

/// The colours the terminal resolves palette references against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub colors: [Rgb; 256],
}

impl Palette {
    /// xterm's default 256-colour palette, with the first 16 replaced by `ansi`.
    pub fn with_ansi(ansi: [Rgb; 16], foreground: Rgb, background: Rgb, cursor: Rgb) -> Self {
        let mut colors = [Rgb::default(); 256];
        colors[..16].copy_from_slice(&ansi);
        // 6x6x6 colour cube.
        let level = |v: usize| if v == 0 { 0 } else { (v * 40 + 55) as u8 };
        for i in 0..216 {
            colors[16 + i] = Rgb::new(level(i / 36), level((i / 6) % 6), level(i % 6));
        }
        // Grayscale ramp.
        for i in 0..24 {
            let v = (8 + i * 10) as u8;
            colors[232 + i] = Rgb::new(v, v, v);
        }
        Self {
            foreground,
            background,
            cursor,
            colors,
        }
    }

    pub fn resolve(&self, color: Color, default: Rgb) -> Rgb {
        match color {
            Color::Default => default,
            Color::Indexed(i) => self.colors[i as usize],
            Color::Rgb(rgb) => rgb,
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        let hex = |s| Rgb::from_hex(s).unwrap();
        Self::with_ansi(
            [
                hex("#000000"),
                hex("#cd0000"),
                hex("#00cd00"),
                hex("#cdcd00"),
                hex("#0000ee"),
                hex("#cd00cd"),
                hex("#00cdcd"),
                hex("#e5e5e5"),
                hex("#7f7f7f"),
                hex("#ff0000"),
                hex("#00ff00"),
                hex("#ffff00"),
                hex("#5c5cff"),
                hex("#ff00ff"),
                hex("#00ffff"),
                hex("#ffffff"),
            ],
            hex("#e5e5e5"),
            hex("#000000"),
            hex("#e5e5e5"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_specs() {
        assert_eq!(Rgb::parse_spec("#ff8000"), Some(Rgb::new(255, 128, 0)));
        assert_eq!(Rgb::parse_spec("rgb:ffff/8080/0000"), Some(Rgb::new(255, 128, 0)));
        assert_eq!(Rgb::parse_spec("rgb:f/8/0"), Some(Rgb::new(255, 136, 0)));
        assert_eq!(Rgb::parse_spec("rgb:ff/80"), None);
        assert_eq!(Rgb::new(255, 128, 0).to_spec(), "rgb:ffff/8080/0000");
    }
}
