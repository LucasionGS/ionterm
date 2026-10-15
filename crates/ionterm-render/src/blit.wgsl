// Copies the linear-light frame to the window surface, encoding it as sRGB.
// The frame is premultiplied, so colour is divided by alpha before encoding
// and multiplied back after; encoding premultiplied values directly would
// brighten every translucent pixel.

@group(0) @binding(0) var frame: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    // One triangle covering the whole viewport.
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    return vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    return select(1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - 0.055, c * 12.92, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let texel = textureLoad(frame, vec2<i32>(position.xy), 0);
    if texel.a <= 0.0 {
        return vec4<f32>(0.0);
    }
    let straight = clamp(texel.rgb / texel.a, vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(linear_to_srgb(straight) * texel.a, texel.a);
}
