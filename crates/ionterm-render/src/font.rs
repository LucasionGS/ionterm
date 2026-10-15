//! Font discovery, shaping, cell metrics and glyph rasterisation, on top of
//! cosmic-text.
//!
//! Text is shaped in runs (a word in one style), so programming ligatures and
//! contextual alternates work, and the result is cached by run text. Glyph
//! bitmaps are cached separately by cosmic-text's `CacheKey`. cosmic-text
//! handles fallback, so a glyph missing from the configured family comes from
//! another installed font.

use std::collections::HashMap;
use std::sync::Arc;

use cosmic_text::{
    Attrs, Buffer, CacheKey, CacheKeyFlags, Family, FontSystem, Metrics, Shaping, Style, SwashCache, SwashContent, Weight,
};

use crate::atlas::{Atlas, AtlasRegion};
use crate::boxdraw;

/// Shaped runs kept before the cache is dropped and rebuilt.
const MAX_CACHED_RUNS: usize = 16_384;

#[derive(Clone, Debug, PartialEq)]
pub struct FontOptions {
    /// Empty = the system monospace font.
    pub family: String,
    /// Size in pixels (already scaled for DPI).
    pub size: f32,
    pub ligatures: bool,
}

/// Pixel measurements every cell shares.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    /// Distance from the top of the cell to the baseline.
    pub baseline: f32,
    /// Distance below the baseline to the top of the underline.
    pub underline_offset: f32,
    pub underline_thickness: f32,
    /// Distance above the baseline to the middle of the strikethrough.
    pub strikeout_offset: f32,
}

/// A rasterised glyph in the atlas.
#[derive(Clone, Copy, Debug)]
pub struct GlyphPiece {
    pub region: AtlasRegion,
    /// Offset from the pen position to the bitmap's left edge.
    pub left: f32,
    /// Offset from the baseline to the bitmap's top edge (positive = up).
    pub top: f32,
    /// Colour glyphs ignore the text colour.
    pub is_color: bool,
}

/// One glyph of a shaped run.
#[derive(Clone, Copy, Debug)]
pub struct ShapedGlyph {
    /// Byte offset of the cluster this glyph belongs to.
    pub cluster: usize,
    pub key: CacheKey,
    /// Pen offset from the cluster's origin.
    pub dx: i32,
    pub dy: i32,
}

/// The atlas is full; the caller should clear it and redraw.
#[derive(Debug)]
pub struct AtlasFull;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RunKey {
    text: String,
    bold: bool,
    italic: bool,
}

pub struct Fonts {
    system: FontSystem,
    swash: SwashCache,
    options: FontOptions,
    metrics: CellMetrics,
    runs: HashMap<RunKey, Arc<[ShapedGlyph]>>,
    /// `None` = the glyph has no pixels (a space).
    rasters: HashMap<CacheKey, Option<GlyphPiece>>,
    procedural: HashMap<char, GlyphPiece>,
}

impl Fonts {
    pub fn new(options: FontOptions) -> Self {
        let mut system = FontSystem::new();
        let metrics = measure(&mut system, &options);
        Self {
            system,
            swash: SwashCache::new(),
            options,
            metrics,
            runs: HashMap::new(),
            rasters: HashMap::new(),
            procedural: HashMap::new(),
        }
    }

    pub fn metrics(&self) -> CellMetrics {
        self.metrics
    }

    pub fn options(&self) -> &FontOptions {
        &self.options
    }

    pub fn ligatures(&self) -> bool {
        self.options.ligatures
    }

    /// Changes family or size. The caller must clear the atlas afterwards.
    pub fn set_options(&mut self, options: FontOptions) {
        if options != self.options {
            self.metrics = measure(&mut self.system, &options);
            self.options = options;
            self.runs.clear();
            self.clear_rasters();
        }
    }

    /// Forgets everything stored in the atlas, after it was cleared.
    pub fn clear_rasters(&mut self) {
        self.rasters.clear();
        self.procedural.clear();
    }

    /// Shapes `text` in the given style. Cheap after the first call.
    pub fn shape(&mut self, text: &str, bold: bool, italic: bool) -> Arc<[ShapedGlyph]> {
        let key = RunKey {
            text: text.to_string(),
            bold,
            italic,
        };
        if let Some(run) = self.runs.get(&key) {
            return run.clone();
        }
        if self.runs.len() >= MAX_CACHED_RUNS {
            self.runs.clear();
        }

        let attrs = attrs(&self.options)
            .weight(if bold { Weight::BOLD } else { Weight::NORMAL })
            .style(if italic { Style::Italic } else { Style::Normal });
        let mut buffer = Buffer::new_empty(Metrics::new(self.options.size, self.metrics.height));
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.system, false);

        let mut glyphs = Vec::new();
        for run in buffer.layout_runs() {
            let mut origin = (usize::MAX, 0.0);
            for glyph in run.glyphs {
                if glyph.start != origin.0 {
                    origin = (glyph.start, glyph.x);
                }
                let physical = glyph.physical((-origin.1, 0.0), 1.0);
                glyphs.push(ShapedGlyph {
                    cluster: glyph.start,
                    key: physical.cache_key,
                    dx: physical.x,
                    dy: physical.y,
                });
            }
        }
        let glyphs: Arc<[ShapedGlyph]> = glyphs.into();
        self.runs.insert(key, glyphs.clone());
        glyphs
    }

    /// The bitmap for a shaped glyph, rasterising it on first use.
    pub fn raster(&mut self, queue: &wgpu::Queue, atlas: &mut Atlas, key: CacheKey) -> Result<Option<GlyphPiece>, AtlasFull> {
        if let Some(piece) = self.rasters.get(&key) {
            return Ok(*piece);
        }
        let piece = match self.swash.get_image_uncached(&mut self.system, key) {
            Some(image) if image.placement.width > 0 && image.placement.height > 0 => {
                let (width, height) = (image.placement.width, image.placement.height);
                let rgba: Vec<u8> = match image.content {
                    SwashContent::Mask => image.data.iter().flat_map(|&a| [255, 255, 255, a]).collect(),
                    SwashContent::Color => image.data.clone(),
                    SwashContent::SubpixelMask => image
                        .data
                        .chunks(4)
                        .flat_map(|px| {
                            let a = ((u16::from(px[0]) + u16::from(px[1]) + u16::from(px[2])) / 3) as u8;
                            [255, 255, 255, a]
                        })
                        .collect(),
                };
                let region = atlas.insert(queue, width, height, &rgba).ok_or(AtlasFull)?;
                Some(GlyphPiece {
                    region,
                    left: image.placement.left as f32,
                    top: image.placement.top as f32,
                    is_color: matches!(image.content, SwashContent::Color),
                })
            }
            _ => None,
        };
        self.rasters.insert(key, piece);
        Ok(piece)
    }

    /// A box-drawing, block or Powerline glyph sized to exactly one cell.
    /// `top` equals the baseline, so it is drawn from the cell's top edge.
    pub fn procedural(&mut self, queue: &wgpu::Queue, atlas: &mut Atlas, c: char) -> Result<Option<GlyphPiece>, AtlasFull> {
        if let Some(piece) = self.procedural.get(&c) {
            return Ok(Some(*piece));
        }
        let m = self.metrics;
        let (width, height) = (m.width as u32, m.height as u32);
        let light = (m.underline_thickness as u32).max(1);
        let Some(mask) = boxdraw::render(c, width, height, light) else {
            return Ok(None);
        };
        let rgba: Vec<u8> = mask.iter().flat_map(|&a| [255, 255, 255, a]).collect();
        let region = atlas.insert(queue, width, height, &rgba).ok_or(AtlasFull)?;
        let piece = GlyphPiece {
            region,
            left: 0.0,
            top: m.baseline,
            is_color: false,
        };
        self.procedural.insert(c, piece);
        Ok(Some(piece))
    }
}

fn attrs(options: &FontOptions) -> Attrs<'_> {
    // swash hints harder than FreeType's "slight" mode that kitty and most
    // Linux apps use through fontconfig, which bulks up strokes. Unhinted
    // outlines match kitty's rendering far more closely.
    let base = Attrs::new().cache_key_flags(CacheKeyFlags::DISABLE_HINTING);
    if options.family.is_empty() {
        base.family(Family::Monospace)
    } else {
        base.family(Family::Name(&options.family))
    }
}

/// Derives the cell size from the primary font: the advance of `M` and the
/// font's ascent, descent and line gap.
fn measure(system: &mut FontSystem, options: &FontOptions) -> CellMetrics {
    let size = options.size;
    let mut buffer = Buffer::new_empty(Metrics::new(size, size * 1.2));
    buffer.set_text("M", &attrs(options), Shaping::Advanced, None);
    buffer.shape_until_scroll(system, false);
    let glyph = buffer.layout_runs().flat_map(|run| run.glyphs.iter()).next().cloned();

    let fallback = CellMetrics {
        width: (size * 0.6).round().max(1.0),
        height: (size * 1.2).ceil(),
        baseline: size.ceil(),
        underline_offset: (size * 0.1).round().max(1.0),
        underline_thickness: (size / 14.0).round().max(1.0),
        strikeout_offset: (size * 0.3).round(),
    };
    let Some(glyph) = glyph else {
        tracing::warn!("no font could shape 'M'; using estimated cell metrics");
        return fallback;
    };
    if let Some(face) = system.db().face(glyph.font_id) {
        let source = match &face.source {
            cosmic_text::fontdb::Source::File(path) | cosmic_text::fontdb::Source::SharedFile(path, _) => {
                path.display().to_string()
            }
            _ => "memory".to_string(),
        };
        let matched = options.family.is_empty()
            || face.families.iter().any(|(name, _)| name.eq_ignore_ascii_case(&options.family));
        if matched {
            tracing::info!("font: {} ({source})", face.post_script_name);
        } else {
            tracing::warn!(
                "font family {:?} not found, falling back to {} ({source})",
                options.family,
                face.post_script_name
            );
        }
    }
    let Some(font) = system.get_font(glyph.font_id, Weight::NORMAL) else {
        return fallback;
    };
    let m = font.metrics();
    let scale = size / f32::from(m.units_per_em.max(1));
    let ascent = m.ascent * scale;
    let descent = -m.descent * scale;
    let leading = m.leading * scale;
    let height = (ascent + descent + leading).ceil().max(1.0);
    let baseline = (ascent + leading / 2.0).round();
    let underline_thickness = m
        .underline
        .map(|d| (d.thickness * scale).round())
        .unwrap_or(fallback.underline_thickness)
        .max(1.0);
    // The font's underline offset is the distance of its centre below the
    // baseline (negative in font units); keep it inside the cell.
    let underline_offset = m
        .underline
        .map(|d| (-d.offset * scale - underline_thickness / 2.0).round())
        .unwrap_or(fallback.underline_offset)
        .clamp(1.0, (height - baseline - underline_thickness).max(1.0));
    let strikeout_offset = m
        .strikeout
        .map(|d| (d.offset * scale).round())
        .or(m.x_height.map(|x| (x * scale / 2.0).round()))
        .unwrap_or(fallback.strikeout_offset);

    CellMetrics {
        width: glyph.w.round().max(1.0),
        height,
        baseline,
        underline_offset,
        underline_thickness,
        strikeout_offset,
    }
}
