//! GPU renderer for ionterm.
//!
//! Draws a [`Snapshot`] with wgpu as a single instanced draw of quads: cell
//! backgrounds first, then glyphs, then decorations and the cursor. The grid
//! is rebuilt from scratch each frame, which is cheap at terminal sizes and
//! keeps the renderer stateless apart from the glyph atlas.

mod atlas;
mod boxdraw;
mod font;
mod images;

use std::sync::Arc;

use anyhow::{Context, anyhow};
use bytemuck::{Pod, Zeroable};
use ionterm_vt::{Cell, Color, CursorShape, Flags, Image, PLACEHOLDER, Palette, Rgb, Snapshot, diacritic_value};
use wgpu::rwh::{HasDisplayHandle, HasWindowHandle};

use atlas::{ATLAS_SIZE, Atlas};
pub use font::{CellMetrics, FontOptions};
use font::{AtlasFull, Fonts, GlyphPiece};
use images::ImageCache;

const KIND_SOLID: u32 = 0;
const KIND_MASK: u32 = 1;
const KIND_COLOR: u32 = 2;
const KIND_ROUNDED: u32 = 3;
const KIND_IMAGE: u32 = 4;
/// The off-screen frame text is blended into: linear light, with headroom.
const FRAME_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// Width of the beam cursor and the outline of the unfocused cursor, in pixels.
const CURSOR_LINE: f32 = 2.0;

/// Visual settings that come from the config and theme rather than the terminal.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderStyle {
    /// Opacity of the default background; other backgrounds stay opaque.
    pub background_opacity: f32,
    pub cursor_shape: CursorShape,
    /// Whether the user's cursor blinks; applications can override it.
    pub cursor_blink: bool,
    /// Text colour under a block cursor. `None` = the background colour.
    pub cursor_text: Option<Rgb>,
    pub selection_foreground: Option<Rgb>,
    pub selection_background: Rgb,
}

/// Per-frame state the app owns.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameState {
    pub focused: bool,
    /// Blink phase; `false` hides a blinking cursor.
    pub cursor_blink_on: bool,
    pub selection: Option<Selection>,
    /// The link under the mouse, underlined.
    pub hovered_link: Option<LinkHighlight>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkHighlight {
    /// Every cell with this OSC 8 hyperlink id.
    Hyperlink(u32),
    /// A span in viewport coordinates, like a selection.
    Span(Selection),
}

/// A picture behind the terminal, filling the window (cropped to fit).
#[derive(Clone, Debug)]
pub struct Background {
    pub image: Image,
    /// How strongly it shows over the background colour, 0–1.
    pub opacity: f32,
}

/// One terminal pane to draw.
pub struct PaneView<'a> {
    pub snapshot: &'a Snapshot,
    /// Top-left corner of the grid, in pixels.
    pub origin: [f32; 2],
    /// The pane's whole area (x, y, width, height) including padding. Filled
    /// when the pane's background differs from the window's.
    pub area: [f32; 4],
    pub frame: FrameState,
}

/// Everything around the panes, drawn on top of them in order.
pub struct Chrome<'a> {
    /// The window's background (theme background).
    pub background: Rgb,
    pub items: &'a [ChromeItem],
}

/// A drawing primitive for window chrome: tab bars, borders, menus.
#[derive(Clone, Debug, PartialEq)]
pub enum ChromeItem {
    Rect {
        rect: [f32; 4],
        color: Rgb,
        alpha: f32,
    },
    RoundedRect {
        rect: [f32; 4],
        radius: f32,
        /// Round only the top corners (a tab).
        top_only: bool,
        color: Rgb,
        alpha: f32,
    },
    /// Monospace text: one cell per column, `y` at the top of the line.
    /// Box-drawing and Powerline characters are drawn procedurally.
    Text {
        text: String,
        x: f32,
        y: f32,
        color: Rgb,
        bold: bool,
    },
    /// Text in another font family, laid out by its own advances: font
    /// previews. Missing glyphs fall back like any text.
    FontText {
        text: String,
        x: f32,
        y: f32,
        color: Rgb,
        family: String,
    },
}

/// Columns a string occupies in the grid (wide characters count twice).
pub fn text_columns(text: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// A selected span in viewport coordinates, start before end. Rows may lie
/// outside the viewport when the selection extends into scrolled-away lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub start: (isize, usize),
    pub end: (isize, usize),
}

impl Selection {
    pub fn contains(&self, row: isize, col: usize) -> bool {
        (row, col) >= self.start && (row, col) <= self.end
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Instance {
    rect: [f32; 4],
    uv: [f32; 4],
    color: [f32; 4],
    kind: u32,
    /// For images: the texture's id and version (see `images::key`).
    extra: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Uniforms {
    screen: [f32; 2],
    atlas_size: [f32; 2],
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    /// The format rendered into; `config.format` without any sRGB suffix.
    view_format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    /// Linear-light frame, encoded to sRGB onto the surface by the blit pass.
    frame_view: wgpu::TextureView,
    blit_pipeline: wgpu::RenderPipeline,
    blit_layout: wgpu::BindGroupLayout,
    blit_bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    instances: wgpu::Buffer,
    instance_capacity: usize,
    atlas: Atlas,
    fonts: Fonts,
    style: RenderStyle,
    supports_transparency: bool,
    scratch: Vec<Instance>,
    images: ImageCache,
    background: Option<Background>,
}

impl Renderer {
    pub fn new<W>(window: Arc<W>, width: u32, height: u32, font: FontOptions, style: RenderStyle) -> anyhow::Result<Self>
    where
        W: HasWindowHandle + HasDisplayHandle + std::fmt::Debug + Send + Sync + 'static,
    {
        let instance = wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(window.clone())),
        );
        let surface = instance.create_surface(window).context("creating the window surface")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("no GPU adapter can draw to this window")?;
        tracing::info!("using GPU adapter: {:?}", adapter.get_info());
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("ionterm"),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .context("creating the GPU device")?;

        let caps = surface.get_capabilities(&adapter);
        // Theme colours are sRGB values written as-is and blended in sRGB
        // space, like every other terminal. That needs a plain 8-bit UNORM
        // target: an `*Srgb` format would encode them a second time, and a
        // float format (Rgba16Float) is read by the compositor as linear
        // light, which washes dark backgrounds out to grey. When only sRGB
        // formats exist, we render through a non-sRGB view of one.
        let format = [wgpu::TextureFormat::Bgra8Unorm, wgpu::TextureFormat::Rgba8Unorm]
            .into_iter()
            .find(|f| caps.formats.contains(f))
            .or_else(|| caps.formats.iter().copied().find(|f| f.is_srgb()))
            .or(caps.formats.first().copied())
            .ok_or_else(|| anyhow!("the window surface reports no formats"))?;
        let view_format = format.remove_srgb_suffix();
        let supports_transparency = caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied);
        let alpha_mode = if supports_transparency {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            caps.alpha_modes[0]
        };
        tracing::debug!("surface formats {:?}, alpha modes {:?}", caps.formats, caps.alpha_modes);
        tracing::info!("surface format {format:?} (drawn as {view_format:?}), alpha {alpha_mode:?}");
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: if view_format == format { vec![] } else { vec![view_format] },
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ionterm shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ionterm bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let image_cache = ImageCache::new(&device, &queue);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ionterm pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout), Some(image_cache.layout())],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ionterm pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x4,
                        1 => Float32x4,
                        2 => Float32x4,
                        3 => Uint32,
                    ],
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FRAME_FORMAT,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ionterm blit shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("blit.wgsl").into()),
        });
        let blit_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("ionterm blit layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let blit_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ionterm blit pipeline layout"),
            bind_group_layouts: &[Some(&blit_layout)],
            immediate_size: 0,
        });
        let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ionterm blit pipeline"),
            layout: Some(&blit_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &blit_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &blit_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: view_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let frame_view = create_frame(&device, config.width, config.height);
        let blit_bind_group = create_blit_bind_group(&device, &blit_layout, &frame_view);

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyph sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let atlas = Atlas::new(&device);
        let bind_group = create_bind_group(&device, &bind_group_layout, &uniforms, &atlas, &sampler);
        let instance_capacity = 4096;
        let instances = create_instance_buffer(&device, instance_capacity);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            view_format,
            pipeline,
            bind_group,
            frame_view,
            blit_pipeline,
            blit_layout,
            blit_bind_group,
            uniforms,
            instances,
            instance_capacity,
            atlas,
            fonts: Fonts::new(font),
            style,
            supports_transparency,
            scratch: Vec::new(),
            images: image_cache,
            background: None,
        })
    }

    pub fn cell_metrics(&self) -> CellMetrics {
        self.fonts.metrics()
    }

    /// The installed monospace font families, for a font picker.
    pub fn monospace_families(&self) -> Vec<String> {
        self.fonts.monospace_families()
    }

    pub fn font_options(&self) -> &FontOptions {
        self.fonts.options()
    }

    pub fn set_font(&mut self, font: FontOptions) {
        // Only when the font changed: clearing the atlas while the glyph
        // cache still points into it would draw stale pieces of it.
        if self.fonts.set_options(font) {
            self.atlas.clear();
        }
    }

    pub fn set_background(&mut self, background: Option<Background>) {
        self.background = background;
    }

    pub fn set_style(&mut self, style: RenderStyle) {
        self.style = style;
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if (width, height) == (self.config.width, self.config.height) {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.frame_view = create_frame(&self.device, width, height);
        self.blit_bind_group = create_blit_bind_group(&self.device, &self.blit_layout, &self.frame_view);
    }

    pub fn render(&mut self, panes: &[PaneView], chrome: &Chrome) {
        let texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            other => {
                tracing::debug!("skipping frame: {other:?}");
                return;
            }
        };

        if let Some(background) = &self.background {
            self.images.prepare(&self.device, &self.queue, &background.image);
        }
        for pane in panes {
            let snapshot = pane.snapshot;
            let images = snapshot.images.iter().map(|d| &d.image);
            for image in images.chain(snapshot.virtual_images.iter().map(|(image, _)| image)) {
                self.images.prepare(&self.device, &self.queue, image);
            }
        }

        let mut instances = std::mem::take(&mut self.scratch);
        instances.clear();
        if !self.build_frame(panes, chrome, &mut instances) {
            // The atlas filled up mid-frame: start it over and rebuild once.
            self.atlas.clear();
            self.fonts.clear_rasters();
            instances.clear();
            self.build_frame(panes, chrome, &mut instances);
        }

        if instances.len() > self.instance_capacity {
            self.instance_capacity = instances.len().next_power_of_two();
            self.instances = create_instance_buffer(&self.device, self.instance_capacity);
        }
        self.queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));
        self.queue.write_buffer(
            &self.uniforms,
            0,
            bytemuck::bytes_of(&Uniforms {
                screen: [self.config.width as f32, self.config.height as f32],
                atlas_size: [ATLAS_SIZE as f32, ATLAS_SIZE as f32],
            }),
        );

        let view = texture.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        });
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ionterm frame"),
        });
        {
            let bg = chrome.background;
            let alpha = if self.supports_transparency {
                f64::from(self.style.background_opacity.clamp(0.0, 1.0))
            } else {
                1.0
            };
            // The frame is linear light, so the clear colour is too.
            let channel = |v: u8| f64::from(srgb_to_linear(f32::from(v) / 255.0)) * alpha;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ionterm pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.frame_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: channel(bg.r),
                            g: channel(bg.g),
                            b: channel(bg.b),
                            a: alpha,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !instances.is_empty() {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.instances.slice(..));
                // One draw per run of instances sharing an image texture
                // (or none): everything else is a single draw.
                let image_of = |i: &Instance| (i.kind == KIND_IMAGE).then_some((i.extra[0], i.extra[1]));
                let mut start = 0;
                while start < instances.len() {
                    let key = image_of(&instances[start]);
                    let end = instances[start..]
                        .iter()
                        .position(|i| image_of(i) != key)
                        .map_or(instances.len(), |n| start + n);
                    let textures = key.and_then(|k| self.images.bind_group(k)).unwrap_or(&self.images.empty);
                    pass.set_bind_group(1, textures, &[]);
                    pass.draw(0..4, start as u32..end as u32);
                    start = end;
                }
            }
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ionterm blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.blit_pipeline);
            pass.set_bind_group(0, &self.blit_bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(texture);
        self.images.end_frame();
        self.scratch = instances;
    }

    /// Appends every quad for the frame. Returns `false` if the atlas ran out
    /// of space part-way through.
    fn build_frame(&mut self, panes: &[PaneView], chrome: &Chrome, out: &mut Vec<Instance>) -> bool {
        let opacity = self.style.background_opacity.clamp(0.0, 1.0);
        if let Some(background) = &self.background {
            let (width, height) = (self.config.width as f32, self.config.height as f32);
            let image = &background.image;
            let (iw, ih) = (image.width as f32, image.height as f32);
            // Cover the window, centred, cropping whatever sticks out.
            let scale = (width / iw).max(height / ih);
            let (w, h) = (iw * scale, ih * scale);
            let dest = [((width - w) / 2.0).round(), ((height - h) / 2.0).round(), w, h];
            push_image(out, image, [0.0, 0.0, iw, ih], dest, [0.0, 0.0, width, height]);
            if let Some(quad) = out.last_mut() {
                quad.color[3] = background.opacity.clamp(0.0, 1.0);
            }
        }
        for pane in panes {
            let bg = pane.snapshot.palette.background;
            if bg != chrome.background {
                out.push(solid(pane.area, bg, opacity));
            }
            if !self.build_pane(pane.snapshot, pane.origin, pane.frame, out) {
                return false;
            }
        }
        for item in chrome.items {
            match item {
                ChromeItem::Rect { rect, color, alpha } => out.push(solid(*rect, *color, *alpha)),
                ChromeItem::RoundedRect {
                    rect,
                    radius,
                    top_only,
                    color,
                    alpha,
                } => out.push(Instance {
                    rect: *rect,
                    uv: [*radius, if *top_only { 1.0 } else { 0.0 }, 0.0, 0.0],
                    color: rgba(*color, *alpha),
                    kind: KIND_ROUNDED,
                    extra: [0; 3],
                }),
                ChromeItem::Text { text, x, y, color, bold } => {
                    if !self.push_text(text, *x, *y, *color, *bold, out) {
                        return false;
                    }
                }
                ChromeItem::FontText { text, x, y, color, family } => {
                    let m = self.fonts.metrics();
                    let glyphs = self.fonts.shape_in(text, family);
                    for glyph in glyphs.iter() {
                        let piece = match self.fonts.raster(&self.queue, &mut self.atlas, glyph.key) {
                            Ok(Some(piece)) => piece,
                            Ok(None) => continue,
                            Err(AtlasFull) => return false,
                        };
                        out.push(glyph_instance(&piece, *x + glyph.dx as f32, *y + m.baseline + glyph.dy as f32, *color));
                    }
                }
            }
        }
        true
    }

    /// Draws a line of text at a pixel position, one cell per column.
    fn push_text(&mut self, text: &str, x: f32, y: f32, color: Rgb, bold: bool, out: &mut Vec<Instance>) -> bool {
        let m = self.fonts.metrics();
        let mut columns = Vec::new();
        let mut run = String::new();
        let mut col = 0;
        for ch in text.chars() {
            let cx = x + col as f32 * m.width;
            if boxdraw::is_procedural(ch) {
                match self.fonts.procedural(&self.queue, &mut self.atlas, ch) {
                    Ok(Some(piece)) => out.push(glyph_instance(&piece, cx, y + m.baseline, color)),
                    Ok(None) => {}
                    Err(AtlasFull) => return false,
                }
                // Keep the shaped run's columns aligned: pad with a space.
                columns.push((run.len(), col));
                run.push(' ');
            } else {
                columns.push((run.len(), col));
                run.push(ch);
            }
            col += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        }
        let glyphs = self.fonts.shape(&run, bold, false);
        for glyph in glyphs.iter() {
            let slot = columns.partition_point(|&(byte, _)| byte <= glyph.cluster).saturating_sub(1);
            let piece = match self.fonts.raster(&self.queue, &mut self.atlas, glyph.key) {
                Ok(Some(piece)) => piece,
                Ok(None) => continue,
                Err(AtlasFull) => return false,
            };
            let gx = x + columns.get(slot).map_or(0, |&(_, col)| col) as f32 * m.width + glyph.dx as f32;
            out.push(glyph_instance(&piece, gx, y + m.baseline + glyph.dy as f32, color));
        }
        true
    }

    fn build_pane(&mut self, snapshot: &Snapshot, origin: [f32; 2], frame: FrameState, out: &mut Vec<Instance>) -> bool {
        let m = self.fonts.metrics();
        let [ox, oy] = origin;
        let palette = &snapshot.palette;

        let cursor = snapshot.cursor.filter(|c| {
            let blinking = c.style.map_or(self.style.cursor_blink, |s| s.blinking);
            !blinking || !frame.focused || frame.cursor_blink_on
        });
        let cursor_shape = cursor
            .and_then(|c| c.style.map(|s| s.shape))
            .unwrap_or(self.style.cursor_shape);
        let block_cursor_at = cursor
            .filter(|_| frame.focused && cursor_shape == CursorShape::Block)
            .map(|c| (c.row, c.col));

        // Glyphs and decorations go after every background, so glyphs that
        // overhang their cell are never painted over by a neighbour.
        let mut overlay = Vec::new();
        let mut fg_colors = Vec::new();
        for (row, line) in snapshot.lines.iter().enumerate() {
            let y = oy + row as f32 * m.height;

            // Backgrounds, and each cell's final text colour.
            fg_colors.clear();
            for (col, cell) in line.cells.iter().enumerate() {
                let x = ox + col as f32 * m.width;
                let (mut fg, mut bg) = cell_colors(cell, palette);
                if frame.selection.is_some_and(|s| s.contains(row as isize, col)) {
                    bg = Some(self.style.selection_background);
                    fg = self.style.selection_foreground.unwrap_or(fg);
                }
                if block_cursor_at == Some((row, col)) {
                    let w = if cell.flags.contains(Flags::WIDE) { m.width * 2.0 } else { m.width };
                    out.push(solid([x, y, w, m.height], palette.cursor, 1.0));
                    fg = self.style.cursor_text.unwrap_or(palette.background);
                } else if let Some(bg) = bg {
                    out.push(solid([x, y, m.width, m.height], bg, 1.0));
                }
                fg_colors.push(fg);
                if !cell.flags.intersects(Flags::WIDE_SPACER | Flags::HIDDEN) {
                    let w = if cell.flags.contains(Flags::WIDE) { m.width * 2.0 } else { m.width };
                    push_decorations(&mut overlay, cell, palette, fg, [x, y, w], &m);
                }
                let hovered = match frame.hovered_link {
                    Some(LinkHighlight::Hyperlink(id)) => cell.hyperlink == id,
                    Some(LinkHighlight::Span(span)) => span.contains(row as isize, col),
                    None => false,
                };
                if hovered && !cell.flags.intersects(Flags::ALL_UNDERLINES) {
                    let underline_y = y + m.baseline + m.underline_offset;
                    overlay.push(solid([x, underline_y, m.width, m.underline_thickness], fg, 1.0));
                }
            }

            // Unicode placeholder cells show slices of virtual image placements.
            if !snapshot.virtual_images.is_empty() {
                push_placeholders(&mut overlay, snapshot, line, [ox, y], &m);
            }

            // Text, in runs of same-style cells so ligatures can form.
            let cells = &line.cells;
            let mut col = 0;
            while col < cells.len() {
                let cell = &cells[col];
                if cell.flags.intersects(Flags::WIDE_SPACER | Flags::HIDDEN) || cell.is_empty() || cell.c == PLACEHOLDER {
                    col += 1;
                    continue;
                }
                if boxdraw::is_procedural(cell.c) && cell.extra.is_none() {
                    match self.fonts.procedural(&self.queue, &mut self.atlas, cell.c) {
                        Ok(Some(piece)) => {
                            overlay.push(glyph_instance(&piece, ox + col as f32 * m.width, y + m.baseline, fg_colors[col]))
                        }
                        Ok(None) => {}
                        Err(AtlasFull) => return false,
                    }
                    col += 1;
                    continue;
                }

                let style = cell.flags & (Flags::BOLD | Flags::ITALIC);
                let mut text = String::new();
                // (byte offset in `text`, column) for every cell in the run.
                let mut columns = Vec::new();
                let mut end = col;
                while end < cells.len() {
                    let c = &cells[end];
                    if c.flags.contains(Flags::WIDE_SPACER) {
                        end += 1;
                        continue;
                    }
                    let joins = c.flags & (Flags::BOLD | Flags::ITALIC) == style
                        && c.c != PLACEHOLDER
                        && !c.flags.contains(Flags::HIDDEN)
                        && !c.is_empty()
                        && !(boxdraw::is_procedural(c.c) && c.extra.is_none());
                    if !joins || (end > col && !self.fonts.ligatures()) {
                        break;
                    }
                    columns.push((text.len(), end));
                    text.push(c.c);
                    if let Some(extra) = &c.extra {
                        text.push_str(extra);
                    }
                    end += 1;
                }

                let glyphs = self.fonts.shape(&text, style.contains(Flags::BOLD), style.contains(Flags::ITALIC));
                for glyph in glyphs.iter() {
                    let slot = columns.partition_point(|&(byte, _)| byte <= glyph.cluster).saturating_sub(1);
                    let glyph_col = columns[slot].1;
                    let piece = match self.fonts.raster(&self.queue, &mut self.atlas, glyph.key) {
                        Ok(Some(piece)) => piece,
                        Ok(None) => continue,
                        Err(AtlasFull) => return false,
                    };
                    let x = ox + glyph_col as f32 * m.width + glyph.dx as f32;
                    let baseline = y + m.baseline + glyph.dy as f32;
                    overlay.push(glyph_instance(&piece, x, baseline, fg_colors[glyph_col]));
                }
                col = end;
            }
        }
        // Images below the text (negative z), the text, then images above it.
        let clip = [ox, oy, snapshot.cols as f32 * m.width, snapshot.rows as f32 * m.height];
        for draw in snapshot.images.iter().filter(|d| d.placement.z < 0) {
            push_placement(out, draw, [ox, oy], clip, &m);
        }
        out.append(&mut overlay);
        for draw in snapshot.images.iter().filter(|d| d.placement.z >= 0) {
            push_placement(out, draw, [ox, oy], clip, &m);
        }

        // Cursor shapes that sit on top of the text.
        if let Some(c) = cursor {
            let x = ox + c.col as f32 * m.width;
            let y = oy + c.row as f32 * m.height;
            let w = if c.wide { m.width * 2.0 } else { m.width };
            let color = palette.cursor;
            if !frame.focused {
                // Hollow block when the window isn't focused.
                out.push(solid([x, y, w, CURSOR_LINE], color, 1.0));
                out.push(solid([x, y + m.height - CURSOR_LINE, w, CURSOR_LINE], color, 1.0));
                out.push(solid([x, y, CURSOR_LINE, m.height], color, 1.0));
                out.push(solid([x + w - CURSOR_LINE, y, CURSOR_LINE, m.height], color, 1.0));
            } else {
                match cursor_shape {
                    CursorShape::Block => {}
                    CursorShape::Beam => out.push(solid([x, y, CURSOR_LINE, m.height], color, 1.0)),
                    CursorShape::Underline => {
                        out.push(solid([x, y + m.height - CURSOR_LINE, w, CURSOR_LINE], color, 1.0))
                    }
                }
            }
        }
        true
    }
}

/// An anchored image placement, clipped to the pane's grid.
fn push_placement(out: &mut Vec<Instance>, draw: &ionterm_vt::ImageDraw, origin: [f32; 2], clip: [f32; 4], m: &CellMetrics) {
    let p = &draw.placement;
    let source = p.source.map(|v| v as f32);
    let x = origin[0] + draw.col as f32 * m.width + p.x_offset as f32;
    let y = origin[1] + draw.row as f32 * m.height + p.y_offset as f32;
    let (w, h) = if p.scaled {
        (p.cols as f32 * m.width, p.rows as f32 * m.height)
    } else {
        (source[2], source[3])
    };
    push_image(out, &draw.image, source, [x, y, w, h], clip);
}

/// Draws the part of `source` (image pixels) that lands inside `clip`, when
/// stretched over `dest`.
fn push_image(out: &mut Vec<Instance>, image: &Image, source: [f32; 4], dest: [f32; 4], clip: [f32; 4]) {
    let [dx, dy, dw, dh] = dest;
    if dw <= 0.0 || dh <= 0.0 {
        return;
    }
    let x0 = dx.max(clip[0]);
    let y0 = dy.max(clip[1]);
    let x1 = (dx + dw).min(clip[0] + clip[2]);
    let y1 = (dy + dh).min(clip[1] + clip[3]);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    // Map the visible part back into the source rectangle.
    let sx = source[0] + (x0 - dx) / dw * source[2];
    let sy = source[1] + (y0 - dy) / dh * source[3];
    let sw = (x1 - x0) / dw * source[2];
    let sh = (y1 - y0) / dh * source[3];
    let (iw, ih) = (image.width as f32, image.height as f32);
    let key = images::key(image);
    out.push(Instance {
        rect: [x0, y0, x1 - x0, y1 - y0],
        uv: [sx / iw, sy / ih, sw / iw, sh / ih],
        color: [1.0; 4],
        kind: KIND_IMAGE,
        extra: [key.0, key.1, 0],
    });
}

/// Draws the image slices for unicode placeholder cells (U+10EEEE). The cell's
/// foreground colour holds the image id, its underline colour the placement
/// id, and combining marks the row and column within the image.
/// `origin` is the top-left corner of the row.
fn push_placeholders(out: &mut Vec<Instance>, snapshot: &Snapshot, line: &ionterm_vt::Row, origin: [f32; 2], m: &CellMetrics) {
    let color_number = |color: Color| match color {
        Color::Rgb(c) => (u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b),
        Color::Indexed(n) => u32::from(n),
        Color::Default => 0,
    };
    // (image id, row, column) of the previous placeholder, which a cell
    // without marks continues from.
    let mut previous: Option<(u32, u32, u32)> = None;
    for (col, cell) in line.cells.iter().enumerate() {
        if cell.c != PLACEHOLDER {
            previous = None;
            continue;
        }
        let low_id = color_number(cell.fg);
        let mut marks = cell.extra.as_deref().unwrap_or_default().chars().filter_map(diacritic_value);
        let (image_row, image_col, high_id) = match (marks.next(), marks.next(), marks.next()) {
            (Some(r), Some(c), high) => (r, c, high.unwrap_or(0)),
            (Some(r), None, _) => match previous {
                Some((id, pr, pc)) if pr == r && id & 0xFF_FFFF == low_id => (r, pc + 1, id >> 24),
                _ => (r, 0, 0),
            },
            (None, ..) => match previous {
                Some((id, pr, pc)) if id & 0xFF_FFFF == low_id => (pr, pc + 1, id >> 24),
                _ => continue,
            },
        };
        let image_id = low_id | (high_id << 24);
        previous = Some((image_id, image_row, image_col));

        let placement_id = color_number(cell.underline_color);
        let Some((image, placement)) = snapshot
            .virtual_images
            .iter()
            .find(|(image, p)| image.id == image_id && (placement_id == 0 || p.placement_id == placement_id))
        else {
            continue;
        };
        // The image is fitted into the placement's box keeping its aspect
        // ratio, centred; each cell shows its share of that box.
        let [sx, sy, sw, sh] = placement.source.map(|v| v as f32);
        let box_w = placement.cols as f32 * m.width;
        let box_h = placement.rows as f32 * m.height;
        let scale = (box_w / sw.max(1.0)).min(box_h / sh.max(1.0));
        let (fit_w, fit_h) = (sw * scale, sh * scale);
        let fit_x = (box_w - fit_w) / 2.0;
        let fit_y = (box_h - fit_h) / 2.0;
        // Where the box's top-left corner would be on screen.
        let box_x = origin[0] + (col as f32 - image_col as f32) * m.width;
        let box_y = origin[1] - image_row as f32 * m.height;
        let cell_rect = [origin[0] + col as f32 * m.width, origin[1], m.width, m.height];
        push_image(out, image, [sx, sy, sw, sh], [box_x + fit_x, box_y + fit_y, fit_w, fit_h], cell_rect);
    }
}

fn glyph_instance(piece: &GlyphPiece, x: f32, baseline: f32, color: Rgb) -> Instance {
    let r = piece.region;
    // On whole pixels: the bitmap maps one texel to one pixel, and a
    // fractional position (UI text at half a cell) would sample between
    // texels, doubling or dropping columns of the glyph.
    Instance {
        rect: [(x + piece.left).round(), (baseline - piece.top).round(), r.width as f32, r.height as f32],
        uv: [r.x as f32, r.y as f32, r.width as f32, r.height as f32],
        color: rgba(color, 1.0),
        kind: if piece.is_color { KIND_COLOR } else { KIND_MASK },
        extra: [0; 3],
    }
}

/// Resolves a cell's foreground and (non-default) background.
fn cell_colors(cell: &Cell, palette: &Palette) -> (Rgb, Option<Rgb>) {
    let mut fg_color = cell.fg;
    // Bold text in one of the 8 base colours uses its bright variant.
    if cell.flags.contains(Flags::BOLD)
        && let ionterm_vt::Color::Indexed(i @ 0..=7) = fg_color
    {
        fg_color = ionterm_vt::Color::Indexed(i + 8);
    }
    let mut fg = palette.resolve(fg_color, palette.foreground);
    let mut bg = match cell.bg {
        ionterm_vt::Color::Default => None,
        other => Some(palette.resolve(other, palette.background)),
    };
    if cell.flags.contains(Flags::INVERSE) {
        let old_fg = fg;
        fg = bg.unwrap_or(palette.background);
        bg = Some(old_fg);
    }
    if cell.flags.contains(Flags::DIM) {
        fg = mix(fg, bg.unwrap_or(palette.background), 0.4);
    }
    (fg, bg)
}

fn push_decorations(out: &mut Vec<Instance>, cell: &Cell, palette: &Palette, fg: Rgb, [x, y, w]: [f32; 3], m: &CellMetrics) {
    let underline_color = match cell.underline_color {
        ionterm_vt::Color::Default => fg,
        other => palette.resolve(other, fg),
    };
    let thickness = m.underline_thickness;
    let underline_y = y + m.baseline + m.underline_offset;
    let flags = cell.flags;
    if flags.contains(Flags::UNDERLINE) {
        out.push(solid([x, underline_y, w, thickness], underline_color, 1.0));
    } else if flags.contains(Flags::DOUBLE_UNDERLINE) {
        out.push(solid([x, underline_y, w, thickness], underline_color, 1.0));
        out.push(solid([x, underline_y + thickness * 2.0, w, thickness], underline_color, 1.0));
    } else if flags.contains(Flags::CURLY_UNDERLINE) {
        // A stepped wave: two segments per cell, alternating height.
        let half = (w / 2.0).floor();
        out.push(solid([x, underline_y, half, thickness], underline_color, 1.0));
        out.push(solid([x + half, underline_y + thickness, w - half, thickness], underline_color, 1.0));
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        let mut dx = 0.0;
        while dx < w {
            out.push(solid([x + dx, underline_y, thickness, thickness], underline_color, 1.0));
            dx += thickness * 2.0;
        }
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        let dash = (w / 3.0).ceil();
        out.push(solid([x, underline_y, dash, thickness], underline_color, 1.0));
        out.push(solid([x + dash * 2.0, underline_y, w - dash * 2.0, thickness], underline_color, 1.0));
    }
    if flags.contains(Flags::STRIKE) {
        let strike_y = y + m.baseline - m.strikeout_offset - thickness / 2.0;
        out.push(solid([x, strike_y.round(), w, thickness], fg, 1.0));
    }
}

fn solid(rect: [f32; 4], color: Rgb, alpha: f32) -> Instance {
    Instance {
        rect,
        uv: [0.0; 4],
        color: rgba(color, alpha),
        kind: KIND_SOLID,
        extra: [0; 3],
    }
}

fn rgba(c: Rgb, alpha: f32) -> [f32; 4] {
    [f32::from(c.r) / 255.0, f32::from(c.g) / 255.0, f32::from(c.b) / 255.0, alpha]
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let lerp = |x: u8, y: u8| (f32::from(x) * (1.0 - t) + f32::from(y) * t).round() as u8;
    Rgb::new(lerp(a.r, b.r), lerp(a.g, b.g), lerp(a.b, b.b))
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn create_frame(device: &wgpu::Device, width: u32, height: u32) -> wgpu::TextureView {
    device
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("linear frame"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FRAME_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
        .create_view(&wgpu::TextureViewDescriptor::default())
}

fn create_blit_bind_group(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, frame: &wgpu::TextureView) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("ionterm blit bind group"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(frame),
        }],
    })
}

fn create_instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("instances"),
        size: (capacity * std::mem::size_of::<Instance>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn create_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    uniforms: &wgpu::Buffer,
    atlas: &Atlas,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("ionterm bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&atlas.view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}
