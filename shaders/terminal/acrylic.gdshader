// SPDX-License-Identifier: GPL-3.0-only
// Acrylic is a frosted neutral surface, separate from the clear 3D tile glass.
// The backdrop is copied before this surface; child text is drawn afterwards.
shader_type canvas_item;
render_mode unshaded;

uniform sampler2D screen_color : hint_screen_texture, repeat_disable, filter_linear_mipmap;
uniform vec2 surface_size = vec2(432.0, 900.0);
uniform vec4 base_color : source_color = vec4(0.968627, 0.968627, 0.968627, 1.0);
// 35% of the backdrop remains visible. This is transmission, not panel opacity.
uniform float background_transmission : hint_range(0.0, 1.0) = 0.35;
uniform float blur_lod : hint_range(0.0, 5.0) = 3.2;
uniform float corner_radius = 0.0;

float grain(vec2 position) {
    return fract(sin(dot(position, vec2(12.9898, 78.233))) * 43758.5453) - 0.5;
}

void fragment() {
    // CanvasItem ancestry supplies fades/tints through incoming COLOR.
    // Preserve it: overwriting COLOR otherwise fades the text but leaves the
    // acrylic sheet fully visible until an alpha-zero engine cull.
    vec4 modulation = COLOR;
    vec2 extent = max(surface_size, vec2(1.0));
    float radius = clamp(corner_radius, 0.0, min(extent.x, extent.y) * 0.5);
    vec2 q = abs((UV - vec2(0.5)) * extent) - extent * 0.5 + radius;
    float edge = length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - radius;
    float mask = 1.0 - smoothstep(-0.75, 0.75, edge);
    vec3 backdrop = textureLod(screen_color, clamp(SCREEN_UV, vec2(0.001), vec2(0.999)), blur_lod).rgb;
    vec3 surface = mix(base_color.rgb, backdrop, background_transmission);
    // Fine, stationary grain avoids a flat alpha wash without sparkling noise.
    surface += vec3(grain(FRAGCOORD.xy) * 0.004);
    COLOR = vec4(clamp(surface, vec3(0.0), vec3(1.0)) * modulation.rgb, mask * modulation.a);
}

// FVPROV1 1818 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNTU5N2EyZDZkNWVmOGJlMWRhNWE3YjRhYzYwMTEzOTE1MmE2Mzc0ODgwNDhlMDkxYWI3OTg0MWY1OGRkNTkyMyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMGE1NmYyYjhhYTI5ZTdlMTU3YmE2YTVlNGEzOTM5MGVhZTkwNjFlMzNkODNmYWM5YTVmMmEyMjQ2NjdiNzNhZGU0ODkyNWVjMmE4YzhkYjU4NGI4Y2Y3YWY3YTQ5Nzk2NzU2MzQ0Nzk1NTQwNDQ0MTkwYTdlY2I1ZDhkZGZjMDAifV19XQ== FVPROV1-END
