//! FSR 1.0 EASU + RCAS for nest pane upscale (AMD FidelityFX, MIT).
//!
//! - `sampler2D` EASU uses `textureGather` (`GL_ARB_texture_gather`).
//! - `samplerExternalOES` EASU uses four point `texture()` taps (no gather/texelFetch).
//! - RCAS always samples a `sampler2D` temp via `texelFetch`, upright, NEAREST.

use glow::UniformLocation;

#[derive(Debug, Default, Clone)]
pub struct Fsr1Locations {
    pub easu_size: Option<UniformLocation>,
    pub easu_radius: Option<UniformLocation>,
    pub easu_flip: Option<UniformLocation>,
    pub easu_origin: Option<UniformLocation>,
    pub easu_src: Option<UniformLocation>,
    pub easu_texel: Option<UniformLocation>,
    pub easu_con0: Option<UniformLocation>,
    pub easu_con1: Option<UniformLocation>,
    pub easu_con2: Option<UniformLocation>,
    pub easu_con3: Option<UniformLocation>,
    pub easu_oes_size: Option<UniformLocation>,
    pub easu_oes_radius: Option<UniformLocation>,
    pub easu_oes_flip: Option<UniformLocation>,
    pub easu_oes_origin: Option<UniformLocation>,
    pub easu_oes_src: Option<UniformLocation>,
    pub easu_oes_texel: Option<UniformLocation>,
    pub easu_oes_con0: Option<UniformLocation>,
    pub easu_oes_con1: Option<UniformLocation>,
    pub easu_oes_con2: Option<UniformLocation>,
    pub easu_oes_con3: Option<UniformLocation>,
    pub rcas_size: Option<UniformLocation>,
    pub rcas_radius: Option<UniformLocation>,
    pub rcas_origin: Option<UniformLocation>,
    pub rcas_con: Option<UniformLocation>,
}

/// CPU-side `FsrEasuCon` — returns float consts (Babylon-style, not AU1 bitcast).
pub fn easu_con(
    input_w: f32,
    input_h: f32,
    output_w: f32,
    output_h: f32,
) -> ([f32; 4], [f32; 4], [f32; 4], [f32; 4]) {
    let input_w = input_w.max(1.0);
    let input_h = input_h.max(1.0);
    let output_w = output_w.max(1.0);
    let output_h = output_h.max(1.0);
    let con0 = [
        input_w / output_w,
        input_h / output_h,
        0.5 * input_w / output_w - 0.5,
        0.5 * input_h / output_h - 0.5,
    ];
    let inv_w = 1.0 / input_w;
    let inv_h = 1.0 / input_h;
    let con1 = [inv_w, inv_h, 1.0 * inv_w, -1.0 * inv_h];
    let con2 = [-1.0 * inv_w, 2.0 * inv_h, 1.0 * inv_w, 2.0 * inv_h];
    let con3 = [0.0 * inv_w, 4.0 * inv_h, 0.0, 0.0];
    (con0, con1, con2, con3)
}

/// CPU-side `FsrRcasCon` — `stops` is {0 = max sharpen … N halving stops}.
pub fn rcas_con(stops: f32) -> [f32; 4] {
    let sharpness = (-stops).exp2();
    [sharpness, sharpness, 0.0, 0.0]
}

fn easu_common_helpers() -> &'static str {
    r#"
float APrxLoRcpF1(float a) { return 1.0 / a; }
float APrxLoRsqF1(float a) { return inversesqrt(a); }
float AMin3F1(float x, float y, float z) { return min(x, min(y, z)); }
float AMax3F1(float x, float y, float z) { return max(x, max(y, z)); }
vec3 AMin3F3(vec3 a, vec3 b, vec3 c) {
    return vec3(AMin3F1(a.r, b.r, c.r), AMin3F1(a.g, b.g, c.g), AMin3F1(a.b, b.b, c.b));
}
vec3 AMax3F3(vec3 a, vec3 b, vec3 c) {
    return vec3(AMax3F1(a.r, b.r, c.r), AMax3F1(a.g, b.g, c.g), AMax3F1(a.b, b.b, c.b));
}

float sdRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

void FsrEasuTapF(
    inout vec3 aC, inout float aW,
    vec2 off, vec2 dir, vec2 len, float lob, float clp, vec3 c
) {
    vec2 v;
    v.x = (off.x * ( dir.x)) + (off.y * dir.y);
    v.y = (off.x * (-dir.y)) + (off.y * dir.x);
    v *= len;
    float d2 = v.x * v.x + v.y * v.y;
    d2 = min(d2, clp);
    float wB = 2.0 / 5.0 * d2 + -1.0;
    float wA = lob * d2 + -1.0;
    wB *= wB;
    wA *= wA;
    wB = 25.0 / 16.0 * wB + -(25.0 / 16.0 - 1.0);
    float w = wB * wA;
    aC += c * w;
    aW += w;
}

void FsrEasuSetF(
    inout vec2 dir, inout float len, vec2 pp,
    bool biS, bool biT, bool biU, bool biV,
    float lA, float lB, float lC, float lD, float lE
) {
    float w = 0.0;
    if (biS) w = (1.0 - pp.x) * (1.0 - pp.y);
    if (biT) w = pp.x * (1.0 - pp.y);
    if (biU) w = (1.0 - pp.x) * pp.y;
    if (biV) w = pp.x * pp.y;
    float dc = lD - lC;
    float cb = lC - lB;
    float lenX = max(abs(dc), abs(cb));
    lenX = APrxLoRcpF1(lenX);
    float dirX = lD - lB;
    dir.x += dirX * w;
    lenX = clamp(abs(dirX) * lenX, 0.0, 1.0);
    lenX *= lenX;
    len += lenX * w;
    float ec = lE - lC;
    float ca = lC - lA;
    float lenY = max(abs(ec), abs(ca));
    lenY = APrxLoRcpF1(lenY);
    float dirY = lE - lA;
    dir.y += dirY * w;
    lenY = clamp(abs(dirY) * lenY, 0.0, 1.0);
    lenY *= lenY;
    len += lenY * w;
}

vec3 FsrEasuF(ivec2 ip, vec4 con0, vec4 con1, vec4 con2, vec4 con3) {
    vec2 pp = vec2(ip) * con0.xy + con0.zw;
    vec2 fp = floor(pp);
    pp -= fp;
    vec2 p0 = fp * con1.xy + con1.zw;
    vec2 p1 = p0 + con2.xy;
    vec2 p2 = p0 + con2.zw;
    vec2 p3 = p0 + con3.xy;
    vec4 bczzR = FsrEasuRF(p0);
    vec4 bczzG = FsrEasuGF(p0);
    vec4 bczzB = FsrEasuBF(p0);
    vec4 ijfeR = FsrEasuRF(p1);
    vec4 ijfeG = FsrEasuGF(p1);
    vec4 ijfeB = FsrEasuBF(p1);
    vec4 klhgR = FsrEasuRF(p2);
    vec4 klhgG = FsrEasuGF(p2);
    vec4 klhgB = FsrEasuBF(p2);
    vec4 zzonR = FsrEasuRF(p3);
    vec4 zzonG = FsrEasuGF(p3);
    vec4 zzonB = FsrEasuBF(p3);
    vec4 bczzL = bczzB * vec4(0.5) + (bczzR * vec4(0.5) + bczzG);
    vec4 ijfeL = ijfeB * vec4(0.5) + (ijfeR * vec4(0.5) + ijfeG);
    vec4 klhgL = klhgB * vec4(0.5) + (klhgR * vec4(0.5) + klhgG);
    vec4 zzonL = zzonB * vec4(0.5) + (zzonR * vec4(0.5) + zzonG);
    float bL = bczzL.x; float cL = bczzL.y;
    float iL = ijfeL.x; float jL = ijfeL.y; float fL = ijfeL.z; float eL = ijfeL.w;
    float kL = klhgL.x; float lL = klhgL.y; float hL = klhgL.z; float gL = klhgL.w;
    float oL = zzonL.z; float nL = zzonL.w;
    vec2 dir = vec2(0.0);
    float len = 0.0;
    FsrEasuSetF(dir, len, pp, true,  false, false, false, bL, eL, fL, gL, jL);
    FsrEasuSetF(dir, len, pp, false, true,  false, false, cL, fL, gL, hL, kL);
    FsrEasuSetF(dir, len, pp, false, false, true,  false, fL, iL, jL, kL, nL);
    FsrEasuSetF(dir, len, pp, false, false, false, true,  gL, jL, kL, lL, oL);
    vec2 dir2 = dir * dir;
    float dirR = dir2.x + dir2.y;
    bool zro = dirR < (1.0 / 32768.0);
    dirR = APrxLoRsqF1(dirR);
    dirR = zro ? 1.0 : dirR;
    dir.x = zro ? 1.0 : dir.x;
    dir *= vec2(dirR);
    len = len * 0.5;
    len *= len;
    float stretch = (dir.x * dir.x + dir.y * dir.y) * APrxLoRcpF1(max(abs(dir.x), abs(dir.y)));
    vec2 len2 = vec2(1.0 + (stretch - 1.0) * len, 1.0 + -0.5 * len);
    float lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    float clp = APrxLoRcpF1(lob);
    vec3 min4 = min(AMin3F3(vec3(ijfeR.z, ijfeG.z, ijfeB.z), vec3(klhgR.w, klhgG.w, klhgB.w), vec3(ijfeR.y, ijfeG.y, ijfeB.y)),
                    vec3(klhgR.x, klhgG.x, klhgB.x));
    vec3 max4 = max(AMax3F3(vec3(ijfeR.z, ijfeG.z, ijfeB.z), vec3(klhgR.w, klhgG.w, klhgB.w), vec3(ijfeR.y, ijfeG.y, ijfeB.y)),
                    vec3(klhgR.x, klhgG.x, klhgB.x));
    vec3 aC = vec3(0.0);
    float aW = 0.0;
    FsrEasuTapF(aC, aW, vec2( 0.0, -1.0) - pp, dir, len2, lob, clp, vec3(bczzR.x, bczzG.x, bczzB.x));
    FsrEasuTapF(aC, aW, vec2( 1.0, -1.0) - pp, dir, len2, lob, clp, vec3(bczzR.y, bczzG.y, bczzB.y));
    FsrEasuTapF(aC, aW, vec2(-1.0,  1.0) - pp, dir, len2, lob, clp, vec3(ijfeR.x, ijfeG.x, ijfeB.x));
    FsrEasuTapF(aC, aW, vec2( 0.0,  1.0) - pp, dir, len2, lob, clp, vec3(ijfeR.y, ijfeG.y, ijfeB.y));
    FsrEasuTapF(aC, aW, vec2( 0.0,  0.0) - pp, dir, len2, lob, clp, vec3(ijfeR.z, ijfeG.z, ijfeB.z));
    FsrEasuTapF(aC, aW, vec2(-1.0,  0.0) - pp, dir, len2, lob, clp, vec3(ijfeR.w, ijfeG.w, ijfeB.w));
    FsrEasuTapF(aC, aW, vec2( 1.0,  1.0) - pp, dir, len2, lob, clp, vec3(klhgR.x, klhgG.x, klhgB.x));
    FsrEasuTapF(aC, aW, vec2( 2.0,  1.0) - pp, dir, len2, lob, clp, vec3(klhgR.y, klhgG.y, klhgB.y));
    FsrEasuTapF(aC, aW, vec2( 2.0,  0.0) - pp, dir, len2, lob, clp, vec3(klhgR.z, klhgG.z, klhgB.z));
    FsrEasuTapF(aC, aW, vec2( 1.0,  0.0) - pp, dir, len2, lob, clp, vec3(klhgR.w, klhgG.w, klhgB.w));
    FsrEasuTapF(aC, aW, vec2( 1.0,  2.0) - pp, dir, len2, lob, clp, vec3(zzonR.z, zzonG.z, zzonB.z));
    FsrEasuTapF(aC, aW, vec2( 0.0,  2.0) - pp, dir, len2, lob, clp, vec3(zzonR.w, zzonG.w, zzonB.w));
    return min(max4, max(min4, aC * vec3(1.0 / aW)));
}
"#
}

fn easu_main() -> &'static str {
    r#"
void main() {
    vec2 p = v_pos * u_size * 0.5;
    vec2 b = u_size * 0.5;
    float distance = sdRoundedBox(p, b, u_radius);
    float smoothing = fwidth(distance);
    float alpha_mask = 1.0 - smoothstep(-smoothing, smoothing, distance);

    ivec2 ip = ivec2(gl_FragCoord.xy) - ivec2(u_origin);
    vec3 pix = FsrEasuF(ip, u_con0, u_con1, u_con2, u_con3);
    f_color = vec4(pix, 1.0) * alpha_mask;
}
"#
}

/// EASU fragment for `sampler2D` — `textureGather` path.
pub fn easu_fragment_2d() -> String {
    format!(
        r#"#version 330 core
#extension GL_ARB_texture_gather : enable
in vec2 v_pos;
in vec2 v_uv;
out vec4 f_color;

uniform sampler2D u_texture;
uniform vec2 u_size;
uniform float u_radius;
uniform int u_flip_y;
uniform vec2 u_origin;
uniform vec2 u_src_size;
uniform vec2 u_texel_size;
uniform vec4 u_con0;
uniform vec4 u_con1;
uniform vec4 u_con2;
uniform vec4 u_con3;

vec2 map_uv(vec2 p) {{
    return (u_flip_y != 0) ? vec2(p.x, 1.0 - p.y) : p;
}}

vec4 FsrEasuRF(vec2 p) {{ return textureGather(u_texture, map_uv(p), 0); }}
vec4 FsrEasuGF(vec2 p) {{ return textureGather(u_texture, map_uv(p), 1); }}
vec4 FsrEasuBF(vec2 p) {{ return textureGather(u_texture, map_uv(p), 2); }}

{helpers}
{main}
"#,
        helpers = easu_common_helpers(),
        main = easu_main()
    )
}

/// EASU fragment for `samplerExternalOES` — four point `texture()` taps, no gather.
pub fn easu_fragment_oes() -> String {
    format!(
        r#"#version 330 core
#extension GL_OES_EGL_image_external_essl3 : enable
#extension GL_OES_EGL_image_external : require
in vec2 v_pos;
in vec2 v_uv;
out vec4 f_color;

uniform samplerExternalOES u_texture;
uniform vec2 u_size;
uniform float u_radius;
uniform int u_flip_y;
uniform vec2 u_origin;
uniform vec2 u_src_size;
uniform vec2 u_texel_size;
uniform vec4 u_con0;
uniform vec4 u_con1;
uniform vec4 u_con2;
uniform vec4 u_con3;

vec2 map_uv(vec2 p) {{
    return (u_flip_y != 0) ? vec2(p.x, 1.0 - p.y) : p;
}}

// Emulate textureGather component order: .x=(0,1) .y=(1,1) .z=(1,0) .w=(0,0)
// with bilinear-free point taps at texel centers.
vec4 gather_comp(vec2 p, int comp) {{
    vec2 base = floor(map_uv(p) * u_src_size - 0.5);
    vec2 t00 = (base + vec2(0.5, 0.5)) * u_texel_size;
    vec2 t10 = (base + vec2(1.5, 0.5)) * u_texel_size;
    vec2 t01 = (base + vec2(0.5, 1.5)) * u_texel_size;
    vec2 t11 = (base + vec2(1.5, 1.5)) * u_texel_size;
    float w = texture(u_texture, t00)[comp];
    float z = texture(u_texture, t10)[comp];
    float x = texture(u_texture, t01)[comp];
    float y = texture(u_texture, t11)[comp];
    return vec4(x, y, z, w);
}}

vec4 FsrEasuRF(vec2 p) {{ return gather_comp(p, 0); }}
vec4 FsrEasuGF(vec2 p) {{ return gather_comp(p, 1); }}
vec4 FsrEasuBF(vec2 p) {{ return gather_comp(p, 2); }}

{helpers}
{main}
"#,
        helpers = easu_common_helpers(),
        main = easu_main()
    )
}

/// RCAS fragment — `texelFetch` on upright `sampler2D` temp. Origin maps pane-local.
pub fn rcas_fragment() -> String {
    r#"#version 330 core
in vec2 v_pos;
in vec2 v_uv;
out vec4 f_color;

uniform sampler2D u_texture;
uniform vec2 u_size;
uniform float u_radius;
uniform vec2 u_origin;
uniform vec4 u_con;

#define FSR_RCAS_LIMIT (0.25-(1.0/16.0))

float APrxMedRcpF1(float a) { return 1.0 / a; }
float AMin3F1(float x, float y, float z) { return min(x, min(y, z)); }
float AMax3F1(float x, float y, float z) { return max(x, max(y, z)); }

float sdRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

vec3 FsrRcasLoadF(ivec2 p) {
    ivec2 mx = textureSize(u_texture, 0) - ivec2(1);
    // AMD: perceptual/sRGB [0,1]; negatives → NaN in lobe math.
    return max(texelFetch(u_texture, clamp(p, ivec2(0), mx), 0).rgb, vec3(0.0));
}

void main() {
    vec2 p = v_pos * u_size * 0.5;
    vec2 b = u_size * 0.5;
    float distance = sdRoundedBox(p, b, u_radius);
    float smoothing = fwidth(distance);
    float alpha_mask = 1.0 - smoothstep(-smoothing, smoothing, distance);

    // 1:1 with upright temp — no Y flip on this pass.
    ivec2 sp = ivec2(gl_FragCoord.xy) - ivec2(u_origin);
    vec3 bC = FsrRcasLoadF(sp + ivec2( 0, -1));
    vec3 dC = FsrRcasLoadF(sp + ivec2(-1,  0));
    vec3 eC = FsrRcasLoadF(sp);
    vec3 fC = FsrRcasLoadF(sp + ivec2( 1,  0));
    vec3 hC = FsrRcasLoadF(sp + ivec2( 0,  1));
    float bR = bC.r; float bG = bC.g; float bB = bC.b;
    float dR = dC.r; float dG = dC.g; float dB = dC.b;
    float eR = eC.r; float eG = eC.g; float eB = eC.b;
    float fR = fC.r; float fG = fC.g; float fB = fC.b;
    float hR = hC.r; float hG = hC.g; float hB = hC.b;
    float bL = bB * 0.5 + (bR * 0.5 + bG);
    float dL = dB * 0.5 + (dR * 0.5 + dG);
    float eL = eB * 0.5 + (eR * 0.5 + eG);
    float fL = fB * 0.5 + (fR * 0.5 + fG);
    float hL = hB * 0.5 + (hR * 0.5 + hG);
    float nz = 0.25 * bL + 0.25 * dL + 0.25 * fL + 0.25 * hL - eL;
    nz = clamp(abs(nz) * APrxMedRcpF1(AMax3F1(AMax3F1(bL, dL, eL), fL, hL) - AMin3F1(AMin3F1(bL, dL, eL), fL, hL)), 0.0, 1.0);
    nz = -0.5 * nz + 1.0;
    float mn4R = min(AMin3F1(bR, dR, fR), hR);
    float mn4G = min(AMin3F1(bG, dG, fG), hG);
    float mn4B = min(AMin3F1(bB, dB, fB), hB);
    float mx4R = max(AMax3F1(bR, dR, fR), hR);
    float mx4G = max(AMax3F1(bG, dG, fG), hG);
    float mx4B = max(AMax3F1(bB, dB, fB), hB);
    vec2 peakC = vec2(1.0, -4.0);
    float hitMinR = min(mn4R, eR) * (1.0 / (4.0 * mx4R));
    float hitMinG = min(mn4G, eG) * (1.0 / (4.0 * mx4G));
    float hitMinB = min(mn4B, eB) * (1.0 / (4.0 * mx4B));
    float hitMaxR = (peakC.x - max(mx4R, eR)) * (1.0 / (4.0 * mn4R + peakC.y));
    float hitMaxG = (peakC.x - max(mx4G, eG)) * (1.0 / (4.0 * mn4G + peakC.y));
    float hitMaxB = (peakC.x - max(mx4B, eB)) * (1.0 / (4.0 * mn4B + peakC.y));
    float lobeR = max(-hitMinR, hitMaxR);
    float lobeG = max(-hitMinG, hitMaxG);
    float lobeB = max(-hitMinB, hitMaxB);
    float lobe = max(-FSR_RCAS_LIMIT, min(AMax3F1(lobeR, lobeG, lobeB), 0.0)) * u_con.x;
    lobe *= nz;
    float rcpL = APrxMedRcpF1(4.0 * lobe + 1.0);
    float pixR = (lobe * bR + lobe * dR + lobe * hR + lobe * fR + eR) * rcpL;
    float pixG = (lobe * bG + lobe * dG + lobe * hG + lobe * fG + eG) * rcpL;
    float pixB = (lobe * bB + lobe * dB + lobe * hB + lobe * fB + eB) * rcpL;
    f_color = vec4(pixR, pixG, pixB, 1.0) * alpha_mask;
}
"#
    .to_string()
}
