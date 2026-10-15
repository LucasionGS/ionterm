//! A single RGBA texture holding every rasterised glyph, packed in shelves.
//!
//! Mask glyphs are stored as white with coverage in alpha, so one texture and
//! one sampler serve both monochrome text and colour emoji.

pub const ATLAS_SIZE: u32 = 2048;
/// Empty pixels between glyphs, so linear sampling never bleeds a neighbour in.
const GAP: u32 = 1;

#[derive(Clone, Copy, Debug)]
pub struct AtlasRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub struct Atlas {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    shelf_x: u32,
    shelf_y: u32,
    shelf_height: u32,
}

impl Atlas {
    pub fn new(device: &wgpu::Device) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            shelf_x: 0,
            shelf_y: 0,
            shelf_height: 0,
        }
    }

    /// Forgets every allocation. Callers must drop their glyph cache too.
    pub fn clear(&mut self) {
        self.shelf_x = 0;
        self.shelf_y = 0;
        self.shelf_height = 0;
    }

    /// Reserves space and uploads `rgba` into it. Returns `None` when the
    /// atlas is full.
    pub fn insert(&mut self, queue: &wgpu::Queue, width: u32, height: u32, rgba: &[u8]) -> Option<AtlasRegion> {
        if width == 0 || height == 0 {
            return Some(AtlasRegion {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            });
        }
        if width + GAP > ATLAS_SIZE || height + GAP > ATLAS_SIZE {
            return None;
        }
        if self.shelf_x + width + GAP > ATLAS_SIZE {
            self.shelf_y += self.shelf_height + GAP;
            self.shelf_x = 0;
            self.shelf_height = 0;
        }
        if self.shelf_y + height + GAP > ATLAS_SIZE {
            return None;
        }
        let region = AtlasRegion {
            x: self.shelf_x,
            y: self.shelf_y,
            width,
            height,
        };
        self.shelf_x += width + GAP;
        self.shelf_height = self.shelf_height.max(height);

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: region.x,
                    y: region.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        Some(region)
    }
}
