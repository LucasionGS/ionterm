//! GPU textures for terminal images (kitty graphics), uploaded once per
//! image version and dropped when unused for a while.

use std::collections::HashMap;

use ionterm_vt::Image;

/// Frames an unused texture is kept before it is freed.
const KEEP_FRAMES: u64 = 600;

/// How an image instance names its texture: id and version.
pub type ImageKey = (u32, u32);

pub fn key(image: &Image) -> ImageKey {
    (image.id, image.generation as u32)
}

struct GpuImage {
    bind_group: wgpu::BindGroup,
    last_used: u64,
}

pub struct ImageCache {
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Bound when drawing anything that isn't an image.
    pub empty: wgpu::BindGroup,
    textures: HashMap<ImageKey, GpuImage>,
    frame: u64,
}

impl ImageCache {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // Images are scaled to fit cells, so they get smooth filtering.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        let empty = upload(device, queue, &layout, &sampler, 1, 1, &[0, 0, 0, 0]);
        Self {
            layout,
            sampler,
            empty,
            textures: HashMap::new(),
            frame: 0,
        }
    }

    pub fn layout(&self) -> &wgpu::BindGroupLayout {
        &self.layout
    }

    /// Makes sure an image has a texture, uploading it on first use.
    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, image: &Image) {
        let key = key(image);
        if let Some(gpu) = self.textures.get_mut(&key) {
            gpu.last_used = self.frame;
            return;
        }
        // An older version of the same image is no longer needed.
        self.textures.retain(|(id, _), _| *id != image.id);
        let max = device.limits().max_texture_dimension_2d;
        let (width, height, pixels) = fit_to_limit(image, max);
        let bind_group = upload(device, queue, &self.layout, &self.sampler, width, height, &pixels);
        self.textures.insert(
            key,
            GpuImage {
                bind_group,
                last_used: self.frame,
            },
        );
    }

    pub fn bind_group(&self, key: ImageKey) -> Option<&wgpu::BindGroup> {
        self.textures.get(&key).map(|gpu| &gpu.bind_group)
    }

    /// Advances the frame counter and frees textures that went unused.
    pub fn end_frame(&mut self) {
        self.frame += 1;
        let frame = self.frame;
        self.textures.retain(|_, gpu| frame - gpu.last_used < KEEP_FRAMES);
    }
}

/// Shrinks an image that exceeds the GPU's texture size limit by an integer
/// factor (nearest neighbour; such images are displayed scaled down anyway).
fn fit_to_limit(image: &Image, max: u32) -> (u32, u32, std::borrow::Cow<'_, [u8]>) {
    let factor = image.width.max(image.height).div_ceil(max.max(1)).max(1);
    if factor == 1 {
        return (image.width, image.height, std::borrow::Cow::Borrowed(&image.rgba[..]));
    }
    let (w, h) = (image.width / factor, image.height / factor);
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let i = (((y * factor) * image.width + x * factor) * 4) as usize;
            out.extend_from_slice(&image.rgba[i..i + 4]);
        }
    }
    (w, h, std::borrow::Cow::Owned(out))
}

fn upload(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> wgpu::BindGroup {
    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("terminal image"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        size,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("image bind group"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}
