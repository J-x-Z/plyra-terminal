// SPDX-License-Identifier: GPL-3.0-only
struct Globals {
    resolution: vec2<f32>,
    interaction: vec2<f32>,
    tint: vec4<f32>,
    profile: vec4<f32>,
    options: vec4<f32>,
};
@group(0) @binding(0) var<uniform> globals: Globals;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) size: vec2<f32>,
    @location(2) @interpolate(flat) state: vec4<f32>,
};

@vertex
fn vertex_main(
    @builtin(vertex_index) vertex: u32,
    @location(0) rect: vec4<f32>,
    @location(1) state: vec4<f32>,
) -> VertexOutput {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(1.0, 0.0)
    );
    let uv = corners[vertex];
    let pixel = rect.xy + uv * rect.zw;
    var output: VertexOutput;
    output.position = vec4<f32>(
        pixel.x / globals.resolution.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.resolution.y * 2.0, 0.0, 1.0
    );
    output.uv = uv;
    output.size = rect.zw;
    output.state = state;
    return output;
}

fn linear_rgb(color: vec3<f32>) -> vec3<f32> {
    return select(color / 12.92, pow((color + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4)), color > vec3<f32>(0.04045));
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let mode = input.state.x;
    if mode < 0.5 {
        return vec4<f32>(linear_rgb(globals.tint.rgb), 1.0);
    }
    let base = clamp(globals.tint.rgb * globals.options.y, vec3<f32>(0.0), vec3<f32>(1.0));
    let tint = linear_rgb(base);
    let alpha = select(1.0, globals.tint.a, input.state.w > 0.5);
    let native_material = globals.options.x > 0.5;
    if mode < 1.5 && native_material {
        return vec4<f32>(tint * alpha, alpha);
    }
    let scale = input.state.z;
    let nearest_edge = min(min(input.uv.x * input.size.x, (1.0 - input.uv.x) * input.size.x),
        min(input.uv.y * input.size.y, (1.0 - input.uv.y) * input.size.y));
    let edge = 1.0 - smoothstep(0.3 * scale, 1.2 * scale, nearest_edge);
    let grain = fract(sin(dot(floor(input.uv * input.size), vec2<f32>(12.9898, 78.233))) * 43758.5453) - 0.5;
    // Pointer-driven light travels only on interaction frames. Reduced motion fixes its position.
    let interactive = globals.interaction.x >= 0.0;
    let phase = select(0.5, dot(globals.interaction, vec2<f32>(0.78, 0.22)), interactive);
    let band = (input.uv.x + input.uv.y * 0.35 - phase * 1.7 + 0.2) / 0.055;
    let sheen = exp(-band * band);
    if native_material {
        var color = base + vec3<f32>(max(0.0, grain) * globals.profile.y);
        color += vec3<f32>(sheen * (globals.profile.z + edge * globals.profile.w));
        return vec4<f32>(linear_rgb(color) * alpha, alpha);
    }
    // The compositor supplies the real backdrop. These terms shade this surface only.
    let detail = select(0.38, 1.0, mode > 1.5);
    let focus = mix(0.75, 1.0, input.state.y);
    let facing = clamp(1.0 - input.uv.x * 0.45 - input.uv.y * 0.55, 0.0, 1.0);
    var color = mix(base, vec3<f32>(1.0), globals.profile.x);
    color += vec3<f32>(grain * globals.profile.y * detail);
    color += vec3<f32>((sheen * globals.profile.z
        + edge * globals.profile.w * (0.3 + facing * 0.7)) * detail * focus);
    return vec4<f32>(linear_rgb(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0))) * alpha, alpha);
}

// FVPROV1 3428 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiOWE5MWQwMzMxODZkMzdjMjNhYWUwZTg2Mjc1NmRlNDU4YmFiNmE3MzA1MjZjZWVjOTE1YThiODIzZGVkZTc5MCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYTgyZDkyMmNmZjBhN2ZkNjliYmYyYTkxZTY1MGQ4NTk1NThmYTZlZjFhM2IyZDhhNDFmOTFkZTQxMWRhNTU0NTk5MGY5YzAwOTU5Yzc1MjFkMTliZDhmMDU5NDM0ZjM1MzQyMWNiZDRmZjBkMGNkNWU2YTdlYjI0ZTg4YWI3MDgifV19XQ== FVPROV1-END
