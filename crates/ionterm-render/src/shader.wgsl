// One instanced quad per background rect, glyph, decoration or cursor part.
// Colours arrive as straight sRGB and are converted to linear light, so glyph
// edges blend the way kitty blends them; blit.wgsl encodes the result back
// to sRGB. The output is premultiplied for the translucent background.

struct Uniforms {
    screen: vec2<f32>,
    atlas_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;
// The image drawn by KIND_IMAGE instances; switched between draw calls.
@group(1) @binding(0) var image_texture: texture_2d<f32>;
@group(1) @binding(1) var image_sampler: sampler;

const KIND_SOLID: u32 = 0u;
const KIND_MASK: u32 = 1u;
const KIND_COLOR: u32 = 2u;
// A rounded rectangle: uv.x = corner radius, uv.y = 1 to round only the top corners.
const KIND_ROUNDED: u32 = 3u;
// A terminal image: uv is already normalised to the image texture.
const KIND_IMAGE: u32 = 4u;

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    return select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c <= vec3<f32>(0.04045));
}

struct Instance {
    // x, y, width, height in pixels.
    @location(0) rect: vec4<f32>,
    // x, y, width, height in atlas pixels.
    @location(1) uv: vec4<f32>,
    @location(2) color: vec4<f32>,
    @location(3) kind: u32,
};

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
    // Position inside the quad in pixels, and its size (for rounded shapes).
    @location(3) local: vec2<f32>,
    @location(4) @interpolate(flat) size: vec2<f32>,
    @location(5) @interpolate(flat) shape: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32, inst: Instance) -> VertexOut {
    // Triangle strip corners: (0,0) (1,0) (0,1) (1,1).
    let corner = vec2<f32>(f32(index & 1u), f32((index >> 1u) & 1u));
    let pixel = inst.rect.xy + corner * inst.rect.zw;
    var out: VertexOut;
    out.position = vec4<f32>(
        pixel.x / u.screen.x * 2.0 - 1.0,
        1.0 - pixel.y / u.screen.y * 2.0,
        0.0,
        1.0,
    );
    let uv = inst.uv.xy + corner * inst.uv.zw;
    out.uv = select(uv / u.atlas_size, uv, inst.kind == KIND_IMAGE);
    out.color = vec4<f32>(srgb_to_linear(inst.color.rgb), inst.color.a);
    out.kind = inst.kind;
    out.local = corner * inst.rect.zw;
    out.size = inst.rect.zw;
    out.shape = inst.uv.xy;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    if in.kind == KIND_SOLID {
        return vec4<f32>(in.color.rgb * in.color.a, in.color.a);
    }
    if in.kind == KIND_ROUNDED {
        let radius = in.shape.x;
        let half = in.size * 0.5;
        var p = in.local - half;
        // Top-only rounding: below the middle, measure as if square.
        if in.shape.y > 0.5 && p.y > 0.0 {
            p.y = min(p.y, half.y - radius);
        }
        let q = abs(p) - (half - vec2<f32>(radius));
        let distance = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - radius;
        let coverage = clamp(0.5 - distance, 0.0, 1.0) * in.color.a;
        return vec4<f32>(in.color.rgb * coverage, coverage);
    }
    if in.kind == KIND_IMAGE {
        let pixel = textureSampleLevel(image_texture, image_sampler, in.uv, 0.0);
        let alpha = pixel.a * in.color.a;
        return vec4<f32>(srgb_to_linear(pixel.rgb) * alpha, alpha);
    }
    let texel = textureSampleLevel(atlas, atlas_sampler, in.uv, 0.0);
    if in.kind == KIND_MASK {
        let alpha = texel.a * in.color.a;
        return vec4<f32>(in.color.rgb * alpha, alpha);
    }
    // Colour glyphs (emoji) keep their own colours.
    return vec4<f32>(srgb_to_linear(texel.rgb) * texel.a, texel.a);
}
