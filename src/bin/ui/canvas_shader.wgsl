struct ViewportUniforms {
    viewport_size: vec2<f32>,
    translation: vec2<f32>,
    scale: vec2<f32>,
    _padding: vec2<f32>,
}

@group(0) @binding(0)
var<uniform> viewport: ViewportUniforms;

struct VertexInput {
    @location(0) position: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
}

@vertex
fn vs_main(vertex: VertexInput) -> VertexOutput {
    let inv_viewport_size = vec2<f32>(2., -2.) / viewport.viewport_size;
    var out: VertexOutput;
    // Apply scale and translation, then convert to normalized device coordinates
    let transformed_position = vertex.position * viewport.scale + viewport.translation;
    let ndc = transformed_position * inv_viewport_size + vec2<f32>(-1., 1.);
    out.clip_position = vec4<f32>(ndc, 0.0, 1.0);
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Light gray grid lines for dark theme
    return vec4<f32>(0.3, 0.3, 0.3, 1.0);
}
