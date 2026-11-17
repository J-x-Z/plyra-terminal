// SPDX-License-Identifier: GPL-3.0-only
struct Globals {
    size_and_radius: vec4<f32>,
};
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var<uniform> globals: Globals;

@vertex
fn vertex_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0)
    );
    return vec4<f32>(positions[index], 0.0, 1.0);
}

@fragment
fn fragment_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let half_size = globals.size_and_radius.xy * 0.5;
    let radius = clamp(globals.size_and_radius.z, 0.0, min(half_size.x, half_size.y));
    let offset = abs(position.xy - half_size) - half_size + vec2<f32>(radius);
    let distance = length(max(offset, vec2<f32>(0.0)))
        + min(max(offset.x, offset.y), 0.0) - radius;
    let coverage = 1.0 - smoothstep(-0.5, 0.5, distance);
    return textureLoad(source, vec2<i32>(position.xy), 0) * coverage;
}

// FVPROV1 1019 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZTQ3YmE4ZDYyZTkyYTJkYzIxNDYwNTExZjFkNjI1ZDUzZDNjODk5MmRhMzUzYWRiMjgwMzFhOGVjZTEzM2Q2MCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMjE0OTczZWRjODVkOGJkYWExZDY3Njc1NmZiZTJjMzA4ZDFmMDYyYWJmZWVhZWJhNzlkOGViODY0Y2U4YWU2OGU5Yjg3NjExYzBlOTM1ZDE3OTY3MjY1YTVhNTU0OTkxNmZkNjJkOWM5ZDgxMWY2YzZhNjJkMjgxOTc5YjUyMDgifV19XQ== FVPROV1-END
