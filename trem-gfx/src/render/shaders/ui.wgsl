// Batched 2D UI. Rects are SDF (fill, CSS border, linear gradient, box-shadow).
// Text is an 8-bit coverage atlas. Images are straight-alpha RGBA.
// Output is premultiplied, blended in gamma space so it matches CSS.
// No MSAA: edges come from the distance field.

struct Globals {
    screen: vec2f,
    _pad: vec2f,
}

struct Instance {
    // Element box in physical pixels (x, y, w, h), origin top-left.
    rect: vec4f,
    // Geometry actually rasterized (expanded for the shadow and AA).
    quad: vec4f,
    // Corner radii: top-left, top-right, bottom-right, bottom-left.
    radii: vec4f,
    color0: vec4f,
    color1: vec4f,
    border: vec4f,
    shadow: vec4f,
    // border width, gradient angle (radians, CSS: 0 = up), blur, spread.
    params: vec4f,
    // shadow offset x, y, unused, unused.
    shadow_off: vec4f,
    // Intersected clip rectangle. Zero size clips everything.
    clip: vec4f,
    // Optional rounded clip. w <= 0 disables it.
    round_box: vec4f,
    round_radii: vec4f,
    // Atlas or image UV rect (u0, v0, u1, v1).
    uv: vec4f,
    tint: vec4f,
}

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<storage, read> instances: array<Instance>;

@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var tex_sampler: sampler;

struct VsOut {
    @builtin(position) pos: vec4f,
    @location(0) uv: vec2f,
    @location(1) @interpolate(flat) index: u32,
}

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    // Field by field, here and in the fragment shaders: `let inst = instances[i]` would copy
    // all 224 bytes per invocation to use a few of them.
    // Two triangles, y down in pixel space: (0,0) (1,0) (0,1) (0,1) (1,0) (1,1).
    var corner = vec2f(0.0, 0.0);
    if vi == 1u || vi == 4u || vi == 5u {
        corner.x = 1.0;
    }
    if vi == 2u || vi == 3u || vi == 5u {
        corner.y = 1.0;
    }
    let quad = instances[ii].quad;
    let uv = instances[ii].uv;
    let px = quad.xy + corner * quad.zw;
    let ndc = vec2f(
        px.x / globals.screen.x * 2.0 - 1.0,
        1.0 - px.y / globals.screen.y * 2.0,
    );
    var out: VsOut;
    out.pos = vec4f(ndc, 0.0, 1.0);
    out.uv = mix(uv.xy, uv.zw, corner);
    out.index = ii;
    return out;
}

fn sd_round_box(p: vec2f, half_size: vec2f, radii: vec4f) -> f32 {
    // p is relative to the box center. +y is down, so p.y < 0 is the top edge.
    let top = select(radii.y, radii.x, p.x < 0.0);
    let bot = select(radii.z, radii.w, p.x < 0.0);
    let r = min(select(bot, top, p.y < 0.0), min(half_size.x, half_size.y));
    let q = abs(p) - half_size + vec2f(r, r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2f(0.0, 0.0))) - r;
}

fn aa(sd: f32) -> f32 {
    // smoothstep requires edge0 < edge1. Coverage is 1 inside the shape (sd < 0).
    let w = max(fwidth(sd), 0.001);
    return 1.0 - smoothstep(-w, w, sd);
}

// Abramowitz and Stegun 7.1.26. erfc(x) = 1 - erf(x).
fn erfc_approx(x: f32) -> f32 {
    let ax = abs(x);
    let t = 1.0 / (1.0 + 0.3275911 * ax);
    let p = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429))));
    let y = p * exp(-ax * ax);
    return select(2.0 - y, y, x >= 0.0);
}

fn gradient(local: vec2f, half_size: vec2f, c0: vec4f, c1: vec4f, angle: f32) -> vec4f {
    // CSS linear-gradient angle: 0deg points up, 90deg points right.
    let dir = vec2f(sin(angle), -cos(angle));
    let denom = abs(dir.x) * half_size.x + abs(dir.y) * half_size.y;
    let t = dot(local, dir) / max(denom, 0.0001);
    return mix(c0, c1, clamp(t * 0.5 + 0.5, 0.0, 1.0));
}

// `aa` takes a derivative, which WGSL allows only where every pixel of the quad gets
// there: both distances are measured first, and `select` keeps what applies.
fn clip_coverage(px: vec2f, index: u32) -> f32 {
    let clip = instances[index].clip;
    let center = clip.xy + clip.zw * 0.5;
    let sd = sd_round_box(px - center, clip.zw * 0.5, vec4f(0.0, 0.0, 0.0, 0.0));
    let round_box = instances[index].round_box;
    let rc = round_box.xy + round_box.zw * 0.5;
    let rsd = sd_round_box(px - rc, round_box.zw * 0.5, instances[index].round_radii);
    let cov = aa(sd);
    let round_cov = aa(rsd);
    let rounded = round_box.z > 0.0 && round_box.w > 0.0;
    let clipped_out = clip.z <= 0.0 || clip.w <= 0.0;
    return select(cov * select(1.0, round_cov, rounded), 0.0, clipped_out);
}

fn over(dst_rgb: vec3f, dst_a: f32, src_rgb: vec3f, src_a: f32) -> vec4f {
    let rgb = src_rgb * src_a + dst_rgb * (1.0 - src_a);
    let a = src_a + dst_a * (1.0 - src_a);
    return vec4f(rgb, a);
}

@fragment
fn fs_rect(in: VsOut) -> @location(0) vec4f {
    let px = in.pos.xy;
    let rect = instances[in.index].rect;
    let params = instances[in.index].params;
    let shadow = instances[in.index].shadow;
    let border = instances[in.index].border;
    let center = rect.xy + rect.zw * 0.5;
    let half_size = rect.zw * 0.5;
    let local = px - center;
    let radii = instances[in.index].radii;
    let sd_outer = sd_round_box(local, half_size, radii);

    let border_w = params.x;
    let inner_half = max(half_size - vec2f(border_w, border_w), vec2f(0.0, 0.0));
    let inner_radii = max(radii - vec4f(border_w, border_w, border_w, border_w), vec4f(0.0, 0.0, 0.0, 0.0));
    let sd_inner = sd_round_box(local, inner_half, inner_radii);

    let fill_cov = aa(sd_outer);
    let border_cov = select(0.0, fill_cov * (1.0 - aa(sd_inner)), border_w > 0.001);
    let fill = gradient(
        local,
        half_size,
        instances[in.index].color0,
        instances[in.index].color1,
        params.y,
    );

    // Measured for every rect, so the derivative in `aa` is taken in uniform control flow.
    let blur = params.z;
    let spread = params.w;
    let off = instances[in.index].shadow_off.xy;
    let shadow_half = max(half_size + vec2f(spread, spread), vec2f(0.0, 0.0));
    let shadow_radii = max(radii + vec4f(spread, spread, spread, spread), vec4f(0.0, 0.0, 0.0, 0.0));
    let sd_shadow = sd_round_box(local - off, shadow_half, shadow_radii);
    let hard_shadow = aa(sd_shadow);
    // CSS blur radius ≈ 2σ (Skia). The visible tail is a few σ wide.
    let sigma = max(blur * 0.5, 0.001);
    let soft_shadow = clamp(0.5 * erfc_approx(sd_shadow / (sigma * 1.41421356)), 0.0, 1.0);
    let shadow_a = select(
        0.0,
        select(soft_shadow, hard_shadow, blur <= 0.001) * shadow.a,
        shadow.a > 0.0,
    );

    var acc = over(vec3f(0.0, 0.0, 0.0), 0.0, shadow.rgb, shadow_a);
    acc = over(acc.rgb, acc.a, fill.rgb, fill.a * fill_cov);
    acc = over(acc.rgb, acc.a, border.rgb, border.a * border_cov);

    let clip_cov = clip_coverage(px, in.index);
    return vec4f(acc.rgb * clip_cov, acc.a * clip_cov);
}

// Lines, SVG's `<line>` stroke. For a line the instance's `rect` holds its two ends
// (x0, y0, x1, y1), `params` the width, the dash, the gap and how far into the dash
// pattern the first end is, `radii.x` 1 for round caps, `color0` the colour; all in
// physical pixels. The quad is the segment's own box, turned along it.
@vertex
fn vs_line(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    var corner = vec2f(0.0, 0.0);
    if vi == 1u || vi == 4u || vi == 5u {
        corner.x = 1.0;
    }
    if vi == 2u || vi == 3u || vi == 5u {
        corner.y = 1.0;
    }
    let ends = instances[ii].rect;
    let d = ends.zw - ends.xy;
    let len = length(d);
    let dir = select(vec2f(1.0, 0.0), d / max(len, 0.0001), len > 0.0001);
    let normal = vec2f(-dir.y, dir.x);
    // Half the width, and a pixel for the edge's coverage.
    let pad = instances[ii].params.x * 0.5 + 1.0;
    let along = mix(-pad, len + pad, corner.x);
    let across = mix(-pad, pad, corner.y);
    let px = ends.xy + dir * along + normal * across;
    var out: VsOut;
    out.pos = vec4f(
        px.x / globals.screen.x * 2.0 - 1.0,
        1.0 - px.y / globals.screen.y * 2.0,
        0.0,
        1.0,
    );
    out.uv = vec2f(along, across);
    out.index = ii;
    return out;
}

@fragment
fn fs_line(in: VsOut) -> @location(0) vec4f {
    let ends = instances[in.index].rect;
    let params = instances[in.index].params;
    let len = length(ends.zw - ends.xy);
    let along = in.uv.x;
    let across = abs(in.uv.y);
    let half = params.x * 0.5;
    // Distance to the stroke's edge, in pixels: a box (butt caps) or a capsule (round).
    let beyond = max(-along, along - len);
    let butt = max(beyond, across - half);
    let round = length(vec2f(max(beyond, 0.0), across)) - half;
    let sd = select(butt, round, instances[in.index].radii.x > 0.5);
    // A pixel's coverage of the edge, as a browser rasterizes a path.
    var cov = clamp(0.5 - sd, 0.0, 1.0);
    // `stroke-dasharray: dash gap`: distance to the nearest dash along the line.
    let dash = params.y;
    let period = dash + params.z;
    if dash > 0.0 && params.z > 0.0 {
        let t = (along + params.w) - floor((along + params.w) / period) * period;
        let to_dash = min(max(-t, t - dash), period - t);
        cov = cov * clamp(0.5 - to_dash, 0.0, 1.0);
    }
    let color = instances[in.index].color0;
    let a = color.a * cov * clip_coverage(in.pos.xy, in.index);
    return vec4f(color.rgb * a, a);
}

@fragment
fn fs_text(in: VsOut) -> @location(0) vec4f {
    let tint = instances[in.index].tint;
    // Blending happens in gamma space, which thins text. Browsers and CoreText counter it
    // with a contrast curve on coverage; this is the same idea, in one exponent.
    let cov = pow(textureSample(tex, tex_sampler, in.uv).r, 0.72);
    let clip_cov = clip_coverage(in.pos.xy, in.index);
    let a = cov * tint.a * clip_cov;
    return vec4f(tint.rgb * a, a);
}

@fragment
fn fs_image(in: VsOut) -> @location(0) vec4f {
    let tint = instances[in.index].tint;
    let texel = textureSample(tex, tex_sampler, in.uv);
    let clip_cov = clip_coverage(in.pos.xy, in.index);
    let rgb = texel.rgb * tint.rgb;
    let a = texel.a * tint.a * clip_cov;
    return vec4f(rgb * a, a);
}
