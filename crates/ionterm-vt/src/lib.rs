//! Terminal state for ionterm.
//!
//! This crate turns the byte stream coming out of a PTY into a grid of styled
//! cells. It knows nothing about windows, fonts or GPUs: the app feeds bytes in
//! through a [`Processor`], drains replies with [`Term::take_output`] and events
//! with [`Term::take_events`], and hands [`Term::snapshot`] to the renderer.

mod base64;
mod cell;
mod charset;
mod color;
mod diacritics;
mod graphics;
mod reflow;
mod sixel;
mod term;

pub use cell::{Cell, Flags, Row};
pub use color::{Color, Palette, Rgb};
pub use diacritics::{PLACEHOLDER, diacritic_value};
pub use graphics::{Image, ImageDraw, Placement};
pub use term::{
    CursorShape, FileRequest, CursorSnapshot, CursorStyle, Processor, Snapshot, Term, TermEvent, TermMode,
};
