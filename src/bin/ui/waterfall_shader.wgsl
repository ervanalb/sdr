struct ViewportUniforms {
    viewport_size: vec2<f32>,
    translation: vec2<f32>,
    scale: vec2<f32>,
    _padding: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> viewport: ViewportUniforms;

@group(0) @binding(1)
var waterfall_texture: texture_2d<f32>;

@group(0) @binding(2)
var prev_waterfall_texture: texture_2d<f32>;

@group(0) @binding(3)
var next_waterfall_texture: texture_2d<f32>;

@group(0) @binding(4)
var waterfall_sampler: sampler;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color_range: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color_range_db: vec2<f32>,
}

@vertex
fn vs_main(vertex: VertexInput) -> VertexOutput {
    let inv_viewport_size = vec2<f32>(2., -2.) / viewport.viewport_size;
    var out: VertexOutput;
    // Apply scale and translation, then convert to normalized device coordinates
    let transformed_position = vertex.position * viewport.scale + viewport.translation;
    let ndc = transformed_position * inv_viewport_size + vec2<f32>(-1., 1.);
    out.clip_position = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = vertex.uv;
    out.color_range_db = 10. * log(vertex.color_range) / log(10.);
    return out;
}

fn jet_colormap(t: f32) -> vec3<f32> {
    return clamp(vec3<f32>(1.5) - abs(4.0 * vec3<f32>(t) + vec3<f32>(-3., -2., -1.)), vec3<f32>(0.), vec3<f32>(1.));
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Sample the waterfall texture

    var value = 0.;

    // TODO: Manually pass in mip level & manually interpolate texture data
    let alpha_prev = 0.5 - in.uv.y * f32(textureDimensions(waterfall_texture).y);
    let prev_value = textureSample(prev_waterfall_texture, waterfall_sampler, vec2<f32>(in.uv.x, in.uv.y + 1.)).r;
    let alpha_next = 0.5 - (1. - in.uv.y) * f32(textureDimensions(waterfall_texture).y);
    let next_value = textureSample(next_waterfall_texture, waterfall_sampler, vec2<f32>(in.uv.x, in.uv.y - 1.)).r;
    let cur_value = textureSample(waterfall_texture, waterfall_sampler, in.uv).r;
    if alpha_prev > 0. {
	value = mix(cur_value, prev_value, alpha_prev);
    } else if alpha_next > 0. {
	value = mix(cur_value, next_value, alpha_next);
    } else {
        value = cur_value;
    }

    // Convert to dB
    let db = 10. * log(value) / log(10.);

    // Convert to color
    let color = jet_colormap((db - in.color_range_db.x) / (in.color_range_db.y - in.color_range_db.x));

    return vec4<f32>(color, 1.0) * 0.2;
}
