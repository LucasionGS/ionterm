use bitflags::bitflags;

use crate::color::Color;

bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct Flags: u16 {
        const BOLD = 1 << 0;
        const DIM = 1 << 1;
        const ITALIC = 1 << 2;
        const UNDERLINE = 1 << 3;
        const DOUBLE_UNDERLINE = 1 << 4;
        const CURLY_UNDERLINE = 1 << 5;
        const DOTTED_UNDERLINE = 1 << 6;
        const DASHED_UNDERLINE = 1 << 7;
        const BLINK = 1 << 8;
        const INVERSE = 1 << 9;
        const HIDDEN = 1 << 10;
        const STRIKE = 1 << 11;
        /// The first column of a double-width character.
        const WIDE = 1 << 12;
        /// The second column of a double-width character; holds no glyph.
        const WIDE_SPACER = 1 << 13;

        const ALL_UNDERLINES = Self::UNDERLINE.bits()
            | Self::DOUBLE_UNDERLINE.bits()
            | Self::CURLY_UNDERLINE.bits()
            | Self::DOTTED_UNDERLINE.bits()
            | Self::DASHED_UNDERLINE.bits();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    /// `Color::Default` means "same as the foreground".
    pub underline_color: Color,
    pub flags: Flags,
    /// OSC 8 hyperlink id (see `Term::hyperlink`); 0 = none.
    pub hyperlink: u32,
    /// Combining characters that follow `c` in the same grapheme. Boxed so the
    /// common case costs one pointer.
    pub extra: Option<Box<str>>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: Color::Default,
            bg: Color::Default,
            underline_color: Color::Default,
            flags: Flags::empty(),
            hyperlink: 0,
            extra: None,
        }
    }
}

impl Cell {
    /// An erased cell. Erasing keeps the current background (back colour
    /// erase), which is what `bce` in the terminfo entry promises.
    pub fn blank(bg: Color) -> Self {
        Self {
            bg,
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        (self.c == ' ' || self.c == '\0') && self.extra.is_none()
    }

    /// The full grapheme stored in this cell.
    pub fn text(&self) -> String {
        let mut s = String::from(self.c);
        if let Some(extra) = &self.extra {
            s.push_str(extra);
        }
        s
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// The line continues on the next row because it soft-wrapped.
    pub wrapped: bool,
    /// A shell prompt starts on this row (OSC 133;A).
    pub prompt: bool,
}

impl Row {
    pub fn new(cols: usize, bg: Color) -> Self {
        Self {
            cells: vec![Cell::blank(bg); cols],
            wrapped: false,
            prompt: false,
        }
    }

    pub fn resize(&mut self, cols: usize) {
        self.cells.resize(cols, Cell::default());
        // A wide character cut in half would leave a glyph with no spacer.
        if let Some(last) = self.cells.last_mut()
            && last.flags.contains(Flags::WIDE)
        {
            *last = Cell::blank(last.bg);
        }
    }
}
