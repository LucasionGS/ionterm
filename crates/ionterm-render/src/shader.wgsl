// One instanced quad per background rect, glyph, decoration or cursor part.
// Colours arrive straight (not premultiplied); the output is premultiplied so
// the compositor blends translucent backgrounds correctly.

struct Uniforms {
    screen: vec2<f32>,
    atlas_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

const KIND_SOLID: u32 = 0u;
const KIND_MASK: u32 = 1u;
const KIND_COLOR: u32 = 2u;

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
    out.uv = (inst.uv.xy + corner * inst.uv.zw) / u.atlas_size;
    out.color = inst.color;
    out.kind = inst.kind;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    if in.kind == KIND_SOLID {
        return vec4<f32>(in.color.rgb * in.color.a, in.color.a);
    }
    let texel = textureSample(atlas, atlas_sampler, in.uv);
    if in.kind == KIND_MASK {
        let alpha = texel.a * in.color.a;
        return vec4<f32>(in.color.rgb * alpha, alpha);
    }
    // Colour glyphs (emoji) keep their own colours.
    return vec4<f32>(texel.rgb * texel.a, texel.a);
}
