//! G0–G3 character sets. Only ASCII and DEC Special Graphics (line drawing)
//! are still used in practice — mostly by ncurses apps drawing borders.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Charset {
    #[default]
    Ascii,
    DecSpecial,
}

impl Charset {
    /// The charset designated by the final byte of `ESC ( F`.
    pub fn from_designator(byte: u8) -> Option<Self> {
        match byte {
            b'B' => Some(Self::Ascii),
            b'0' => Some(Self::DecSpecial),
            _ => None,
        }
    }

    pub fn map(self, c: char) -> char {
        match self {
            Self::Ascii => c,
            Self::DecSpecial => match c {
                '_' => ' ',
                '`' => '◆',
                'a' => '▒',
                'b' => '␉',
                'c' => '␌',
                'd' => '␍',
                'e' => '␊',
                'f' => '°',
                'g' => '±',
                'h' => '␤',
                'i' => '␋',
                'j' => '┘',
                'k' => '┐',
                'l' => '┌',
                'm' => '└',
                'n' => '┼',
                'o' => '⎺',
                'p' => '⎻',
                'q' => '─',
                'r' => '⎼',
                's' => '⎽',
                't' => '├',
                'u' => '┤',
                'v' => '┴',
                'w' => '┬',
                'x' => '│',
                'y' => '≤',
                'z' => '≥',
                '{' => 'π',
                '|' => '≠',
                '}' => '£',
                '~' => '·',
                _ => c,
            },
        }
    }
}
