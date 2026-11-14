use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use dpi::PhysicalSize as ServoPhysicalSize;
use euclid::Point2D;
use napi_derive::napi;
use serde::{Deserialize, Serialize};
use servo::{Code, Key, Modifiers, RenderingContext, WindowRenderingContext};

use crate::gl::LotusPaneContext;
use winit::window::Window;

use crate::layout::{
    ActiveGutterDrag, HitTestGutter, LayoutNode, PaneAnchor, StagedLayout, StagedPaneLayout,
};

pub const STENCIL_VS: &str = r#"
#version 330 core
const vec2 verts[4] = vec2[4](vec2(-1.0, -1.0), vec2(1.0, -1.0), vec2(-1.0, 1.0), vec2(1.0, 1.0));
out vec2 v_pos;
void main() {
    vec2 pos = verts[gl_VertexID];
    v_pos = pos;
    gl_Position = vec4(pos, 0.0, 1.0);
}
"#;

pub const STENCIL_FS: &str = r#"
#version 330 core
in vec2 v_pos;
out vec4 FragColor;
uniform vec2 u_size;
uniform float u_radius;
void main() {
    vec2 px = (v_pos * 0.5 + 0.5) * u_size;
    vec2 half_size = u_size * 0.5;
    vec2 d = abs(px - half_size) - half_size + vec2(u_radius);
    float dist = length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - u_radius;
    if (dist > 0.0) discard;
    FragColor = vec4(1.0);
}
"#;

pub const COMP_VS: &str = r#"
#version 330 core
layout (location = 0) in vec2 a_pos;
out vec2 v_pos;
out vec2 v_uv;
void main() {
    v_pos = a_pos;
    v_uv = a_pos * 0.5 + 0.5;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
"#;

pub const COMP_FS: &str = r#"
#version 330 core
in vec2 v_pos;
in vec2 v_uv;
out vec4 f_color;

uniform sampler2D u_texture;
uniform vec2 u_size;
uniform float u_radius;
uniform int u_flip_y;

float sdRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

void main() {
    // p is in pixels, centered at 0,0
    vec2 p = v_pos * u_size * 0.5;
    vec2 b = u_size * 0.5;
    
    float distance = sdRoundedBox(p, b, u_radius);
    float smoothing = fwidth(distance); // Auto-adjusts for HiDPI/Scaling
    float alpha_mask = 1.0 - smoothstep(-smoothing, smoothing, distance);
    
    vec2 uv = (u_flip_y != 0) ? vec2(v_uv.x, 1.0 - v_uv.y) : v_uv;
    vec4 tex_color = texture(u_texture, uv);
    // Premultiplied alpha blending: multiply texture alpha by the mask
    f_color = tex_color * alpha_mask;
}
"#;

/// Desktop GL compose for Proton/Xwayland DMA-BUFs imported as GL_TEXTURE_EXTERNAL_OES.
/// Mesa RadeonSI/Zink need GL_OES_EGL_image_external_essl3 for samplerExternalOES in #version 330.
/// Linux-only: nest DMA-BUF import does not exist on Windows.
#[cfg(target_os = "linux")]
pub const COMP_FS_OES: &str = r#"
#version 330 core
#extension GL_OES_EGL_image_external_essl3 : enable
#extension GL_OES_EGL_image_external : require
in vec2 v_pos;
in vec2 v_uv;
out vec4 f_color;

uniform samplerExternalOES u_texture;
uniform vec2 u_size;
uniform float u_radius;
uniform int u_flip_y;

float sdRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

void main() {
    vec2 p = v_pos * u_size * 0.5;
    vec2 b = u_size * 0.5;

    float distance = sdRoundedBox(p, b, u_radius);
    float smoothing = fwidth(distance);
    float alpha_mask = 1.0 - smoothstep(-smoothing, smoothing, distance);

    vec2 uv = (u_flip_y != 0) ? vec2(v_uv.x, 1.0 - v_uv.y) : v_uv;
    vec4 tex_color = texture(u_texture, uv);
    f_color = tex_color * alpha_mask;
}
"#;

/// Fragment adaptation of gamescope's `src/shaders/sgsr1.h`.
///
/// Copyright (c) Qualcomm Technologies, Inc. and/or its subsidiaries.
/// SPDX-License-Identifier: BSD-3-Clause
/// Gamescope adapts Qualcomm SGSR1 (`sgsr1_shader_mobile.frag`). On an edge it
/// *replaces* each channel with a 12-tap Lanczos gather, clamped to the local
/// 2x2. The previous port only added a ±23/255 luma nudge on top of bilinear,
/// which is why SGSR looked the same as linear.
///
/// `sampler2D` emulates `textureGather` with `texelFetch` — GL 3.3 does not
/// expose gather from `GL_ARB_texture_gather` alone (needs gpu_shader5 / 400),
/// and a failed link silently fell back to bilinear so Off/SGSR looked identical.
/// `samplerExternalOES` uses four point `texture()` taps. Source stays LINEAR
/// for the bilinear base sample. `u_edge_sharpness` 2 → gamescope kernel 0.8.
#[cfg(target_os = "linux")]
pub fn sgsr_fragment_source(external: bool) -> String {
    let extensions = if external {
        "#extension GL_OES_EGL_image_external_essl3 : enable\n\
         #extension GL_OES_EGL_image_external : require\n"
    } else {
        ""
    };
    let sampler = if external {
        "samplerExternalOES"
    } else {
        "sampler2D"
    };
    // GL textureGather order: .x=(0,1) .y=(1,1) .z=(1,0) .w=(0,0)
    let gather = if external {
        r#"
vec4 sgsrGatherComp(vec2 p, int comp) {
    vec2 base = floor(p * u_src_size - 0.5);
    float w = texture(u_texture, (base + vec2(0.5, 0.5)) * u_texel_size)[comp];
    float z = texture(u_texture, (base + vec2(1.5, 0.5)) * u_texel_size)[comp];
    float x = texture(u_texture, (base + vec2(0.5, 1.5)) * u_texel_size)[comp];
    float y = texture(u_texture, (base + vec2(1.5, 1.5)) * u_texel_size)[comp];
    return vec4(x, y, z, w);
}
vec4 sgsrGatherR(vec2 p) { return sgsrGatherComp(p, 0); }
vec4 sgsrGatherG(vec2 p) { return sgsrGatherComp(p, 1); }
vec4 sgsrGatherB(vec2 p) { return sgsrGatherComp(p, 2); }
"#
    } else {
        r#"
vec4 sgsrGatherComp(vec2 p, int comp) {
    ivec2 texSize = textureSize(u_texture, 0);
    ivec2 mx = texSize - ivec2(1);
    ivec2 base = ivec2(floor(p * vec2(texSize) - 0.5));
    return vec4(
        texelFetch(u_texture, clamp(base + ivec2(0, 1), ivec2(0), mx), 0)[comp],
        texelFetch(u_texture, clamp(base + ivec2(1, 1), ivec2(0), mx), 0)[comp],
        texelFetch(u_texture, clamp(base + ivec2(1, 0), ivec2(0), mx), 0)[comp],
        texelFetch(u_texture, clamp(base + ivec2(0, 0), ivec2(0), mx), 0)[comp]
    );
}
vec4 sgsrGatherR(vec2 p) { return sgsrGatherComp(p, 0); }
vec4 sgsrGatherG(vec2 p) { return sgsrGatherComp(p, 1); }
vec4 sgsrGatherB(vec2 p) { return sgsrGatherComp(p, 2); }
"#
    };
    format!(
        r#"#version 330 core
{extensions}in vec2 v_pos;
in vec2 v_uv;
out vec4 f_color;

uniform {sampler} u_texture;
uniform vec2 u_size;
uniform float u_radius;
uniform int u_flip_y;
uniform vec2 u_texel_size;
uniform vec2 u_src_size;
uniform float u_edge_sharpness;
uniform float u_edge_threshold;
uniform int u_use_edge_direction;

float sdRoundedBox(vec2 p, vec2 b, float r) {{
    vec2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}}

{gather}
float sgsrFastLanczos2(float x) {{
    x = min(x, 4.0);
    float wA = x - 4.0;
    float wB = x * wA - wA;
    wA *= wA;
    return wB * wA;
}}

float sgsrWeight(float dx, float dy, float g, float std) {{
    float d2 = dx * dx + dy * dy;
    float outlier = clamp(abs(g) * std, 0.0, 1.0) * min(d2 * 0.5, 1.0);
    // sharpness 2 → gamescope's 0.8. Larger width drops far taps sooner.
    float kernel = clamp(u_edge_sharpness * 0.4, 0.2, 1.6);
    return sgsrFastLanczos2(d2 * kernel + outlier);
}}

float sgsrChannel(float w0, float w1, float w2, float w3, float w4, float w5,
                  float w6, float w7, float w8, float w9, float w10, float w11,
                  vec4 ud, vec4 l, vec4 r, float invSum) {{
    float acc = dot(vec4(w0, w1, w2, w3), ud)
              + dot(vec4(w4, w5, w6, w7), l)
              + dot(vec4(w8, w9, w10, w11), r);
    float lo = min(min(l.y, l.z), min(r.x, r.w));
    float hi = max(max(l.y, l.z), max(r.x, r.w));
    return clamp(acc * invSum, lo, hi);
}}

vec4 sampleSgsr1(vec2 uv) {{
    vec4 color = texture(u_texture, uv);
    if (u_use_edge_direction == 0) {{
        return color;
    }}
    vec4 viewport = vec4(1.0 / u_src_size, u_src_size);
    vec2 imgCoord = (uv * viewport.zw) + vec2(-0.5, 0.5);
    vec2 imgCoordPixel = floor(imgCoord);
    vec2 coord = imgCoordPixel * viewport.xy;
    vec2 pl = imgCoord - imgCoordPixel;
    vec4 leftG = sgsrGatherG(coord);
    float edgeVote = abs(leftG.z - leftG.y) + abs(color.g - leftG.y) + abs(color.g - leftG.z);
    if (edgeVote > u_edge_threshold) {{
        vec2 leftCoord = coord;
        coord.x += viewport.x;
        vec2 rightCoord = coord + vec2(viewport.x, 0.0);
        vec2 upCoord = coord + vec2(0.0, -viewport.y);
        vec2 downCoord = coord + vec2(0.0, viewport.y);
        vec4 rightG = sgsrGatherG(rightCoord);
        vec4 upDownG;
        upDownG.xy = sgsrGatherG(upCoord).wz;
        upDownG.zw = sgsrGatherG(downCoord).yx;

        float meanG = (leftG.y + leftG.z + rightG.x + rightG.w) * 0.25;
        leftG -= vec4(meanG);
        rightG -= vec4(meanG);
        upDownG -= vec4(meanG);

        float sum = dot(abs(leftG) + abs(rightG) + abs(upDownG), vec4(1.0));
        float std = 2.181818 / max(sum, 1e-5);

        float w0 = sgsrWeight(pl.x,       pl.y + 1.0, upDownG.x, std);
        float w1 = sgsrWeight(pl.x - 1.0, pl.y + 1.0, upDownG.y, std);
        float w2 = sgsrWeight(pl.x - 1.0, pl.y - 2.0, upDownG.z, std);
        float w3 = sgsrWeight(pl.x,       pl.y - 2.0, upDownG.w, std);
        float w4 = sgsrWeight(pl.x + 1.0, pl.y - 1.0, leftG.x,    std);
        float w5 = sgsrWeight(pl.x,       pl.y - 1.0, leftG.y,    std);
        float w6 = sgsrWeight(pl.x,       pl.y,       leftG.z,    std);
        float w7 = sgsrWeight(pl.x + 1.0, pl.y,       leftG.w,    std);
        float w8 = sgsrWeight(pl.x - 1.0, pl.y - 1.0, rightG.x,   std);
        float w9 = sgsrWeight(pl.x - 2.0, pl.y - 1.0, rightG.y,   std);
        float w10 = sgsrWeight(pl.x - 2.0, pl.y,      rightG.z,   std);
        float w11 = sgsrWeight(pl.x - 1.0, pl.y,      rightG.w,   std);
        float invSum = 1.0 / max((w0+w1)+(w2+w3)+(w4+w5)+(w6+w7)+(w8+w9)+(w10+w11), 1e-5);

        color.g = meanG + sgsrChannel(w0, w1, w2, w3, w4, w5, w6, w7, w8, w9, w10, w11,
                                       upDownG, leftG, rightG, invSum);
        vec4 l = sgsrGatherR(leftCoord);
        vec4 r = sgsrGatherR(rightCoord);
        vec4 ud;
        ud.xy = sgsrGatherR(upCoord).wz;
        ud.zw = sgsrGatherR(downCoord).yx;
        color.r = sgsrChannel(w0, w1, w2, w3, w4, w5, w6, w7, w8, w9, w10, w11,
                               ud, l, r, invSum);
        l = sgsrGatherB(leftCoord);
        r = sgsrGatherB(rightCoord);
        ud.xy = sgsrGatherB(upCoord).wz;
        ud.zw = sgsrGatherB(downCoord).yx;
        color.b = sgsrChannel(w0, w1, w2, w3, w4, w5, w6, w7, w8, w9, w10, w11,
                               ud, l, r, invSum);
    }}
    return color;
}}

void main() {{
    vec2 p = v_pos * u_size * 0.5;
    vec2 b = u_size * 0.5;
    float distance = sdRoundedBox(p, b, u_radius);
    float smoothing = fwidth(distance);
    float alpha_mask = 1.0 - smoothstep(-smoothing, smoothing, distance);

    vec2 uv = (u_flip_y != 0) ? vec2(v_uv.x, 1.0 - v_uv.y) : v_uv;
    vec4 color = sampleSgsr1(uv);
    f_color = vec4(color.rgb, color.a) * alpha_mask;
}}
"#
        ,
        extensions = extensions,
        sampler = sampler,
        gather = gather
    )
}

// Host upscaler config lives in host_upscale.rs (mutable + lazy shader ensure).
#[cfg(target_os = "linux")]
pub use crate::host_upscale::{
    fsr1_upscale_enabled, host_sharpen_rcas, host_upscale, host_upscale_config,
    rcas_sharpness_stops, set_host_upscale_config, sgsr_edge_sharpness, sgsr_edge_threshold,
    sgsr_upscale_enabled, sgsr_use_edge_direction, HostUpscale, HostUpscaleConfig,
};

#[cfg(not(target_os = "linux"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostUpscale {
    #[default]
    Off,
    Sgsr,
    Fsr1,
}

#[cfg(not(target_os = "linux"))]
pub fn host_upscale() -> HostUpscale {
    HostUpscale::Off
}

#[cfg(not(target_os = "linux"))]
pub fn host_sharpen_rcas() -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
pub fn rcas_sharpness_stops() -> f32 {
    0.2
}

#[derive(Debug, Clone)]
pub struct WindowMetadata {
    pub root_path: Option<PathBuf>,
    pub last_window_size: Option<winit::dpi::PhysicalSize<u32>>,
}

pub fn map_winit_modifiers(winit_mods: winit::event::Modifiers) -> Modifiers {
    let mut s_mods = Modifiers::empty();
    let state = winit_mods.state();
    if state.shift_key() { s_mods.insert(Modifiers::SHIFT); }
    if state.control_key() { s_mods.insert(Modifiers::CONTROL); }
    if state.alt_key() { s_mods.insert(Modifiers::ALT); }
    if state.super_key() { s_mods.insert(Modifiers::META); }
    s_mods
}

/// Host hotkey modifier bitmask (lock keys ignored at match time).
#[cfg(target_os = "linux")]
pub const HOST_HOTKEY_CTRL: u32 = 1;
#[cfg(target_os = "linux")]
pub const HOST_HOTKEY_ALT: u32 = 2;
#[cfg(target_os = "linux")]
pub const HOST_HOTKEY_SHIFT: u32 = 4;
#[cfg(target_os = "linux")]
pub const HOST_HOTKEY_SUPER: u32 = 8;

/// One registered host hotkey: W3C `KeyboardEvent.code` → physical KeyCode + mod mask.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub struct HostHotkeyBinding {
    pub id: String,
    pub key: winit::keyboard::KeyCode,
    pub modifiers: u32,
}

/// Parse a W3C `KeyboardEvent.code` string into a winit `KeyCode`.
#[cfg(target_os = "linux")]
pub fn parse_keyboard_event_code(code: &str) -> Option<winit::keyboard::KeyCode> {
    use winit::keyboard::KeyCode;
    Some(match code {
        "F1" => KeyCode::F1,
        "F2" => KeyCode::F2,
        "F3" => KeyCode::F3,
        "F4" => KeyCode::F4,
        "F5" => KeyCode::F5,
        "F6" => KeyCode::F6,
        "F7" => KeyCode::F7,
        "F8" => KeyCode::F8,
        "F9" => KeyCode::F9,
        "F10" => KeyCode::F10,
        "F11" => KeyCode::F11,
        "F12" => KeyCode::F12,
        "F13" => KeyCode::F13,
        "F14" => KeyCode::F14,
        "F15" => KeyCode::F15,
        "F16" => KeyCode::F16,
        "F17" => KeyCode::F17,
        "F18" => KeyCode::F18,
        "F19" => KeyCode::F19,
        "F20" => KeyCode::F20,
        "F21" => KeyCode::F21,
        "F22" => KeyCode::F22,
        "F23" => KeyCode::F23,
        "F24" => KeyCode::F24,
        "Escape" => KeyCode::Escape,
        "Backquote" => KeyCode::Backquote,
        "Digit0" => KeyCode::Digit0,
        "Digit1" => KeyCode::Digit1,
        "Digit2" => KeyCode::Digit2,
        "Digit3" => KeyCode::Digit3,
        "Digit4" => KeyCode::Digit4,
        "Digit5" => KeyCode::Digit5,
        "Digit6" => KeyCode::Digit6,
        "Digit7" => KeyCode::Digit7,
        "Digit8" => KeyCode::Digit8,
        "Digit9" => KeyCode::Digit9,
        "KeyA" => KeyCode::KeyA,
        "KeyB" => KeyCode::KeyB,
        "KeyC" => KeyCode::KeyC,
        "KeyD" => KeyCode::KeyD,
        "KeyE" => KeyCode::KeyE,
        "KeyF" => KeyCode::KeyF,
        "KeyG" => KeyCode::KeyG,
        "KeyH" => KeyCode::KeyH,
        "KeyI" => KeyCode::KeyI,
        "KeyJ" => KeyCode::KeyJ,
        "KeyK" => KeyCode::KeyK,
        "KeyL" => KeyCode::KeyL,
        "KeyM" => KeyCode::KeyM,
        "KeyN" => KeyCode::KeyN,
        "KeyO" => KeyCode::KeyO,
        "KeyP" => KeyCode::KeyP,
        "KeyQ" => KeyCode::KeyQ,
        "KeyR" => KeyCode::KeyR,
        "KeyS" => KeyCode::KeyS,
        "KeyT" => KeyCode::KeyT,
        "KeyU" => KeyCode::KeyU,
        "KeyV" => KeyCode::KeyV,
        "KeyW" => KeyCode::KeyW,
        "KeyX" => KeyCode::KeyX,
        "KeyY" => KeyCode::KeyY,
        "KeyZ" => KeyCode::KeyZ,
        "Tab" => KeyCode::Tab,
        "Space" => KeyCode::Space,
        "Enter" => KeyCode::Enter,
        "Backspace" => KeyCode::Backspace,
        "Delete" => KeyCode::Delete,
        "Insert" => KeyCode::Insert,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "ArrowLeft" => KeyCode::ArrowLeft,
        "ArrowRight" => KeyCode::ArrowRight,
        "ArrowUp" => KeyCode::ArrowUp,
        "ArrowDown" => KeyCode::ArrowDown,
        "Minus" => KeyCode::Minus,
        "Equal" => KeyCode::Equal,
        "BracketLeft" => KeyCode::BracketLeft,
        "BracketRight" => KeyCode::BracketRight,
        "Backslash" => KeyCode::Backslash,
        "Semicolon" => KeyCode::Semicolon,
        "Quote" => KeyCode::Quote,
        "Comma" => KeyCode::Comma,
        "Period" => KeyCode::Period,
        "Slash" => KeyCode::Slash,
        _ => return None,
    })
}

/// Active Ctrl/Alt/Shift/Super only — CapsLock/NumLock/ScrollLock ignored.
#[cfg(target_os = "linux")]
pub fn host_hotkey_mod_mask(mods: Modifiers) -> u32 {
    let mut m = 0u32;
    if mods.contains(Modifiers::CONTROL) {
        m |= HOST_HOTKEY_CTRL;
    }
    if mods.contains(Modifiers::ALT) {
        m |= HOST_HOTKEY_ALT;
    }
    if mods.contains(Modifiers::SHIFT) {
        m |= HOST_HOTKEY_SHIFT;
    }
    if mods.contains(Modifiers::META) {
        m |= HOST_HOTKEY_SUPER;
    }
    m
}

pub fn map_winit_key(winit_key: &winit::keyboard::Key) -> Key {
    use winit::keyboard::Key as WKey;
    use winit::keyboard::NamedKey as WNamed;
    use servo::NamedKey as SKey;
    match winit_key {
        WKey::Character(s) => Key::Character(s.to_string()),
        WKey::Named(n) => match n {
            WNamed::Backspace => Key::Named(SKey::Backspace),
            WNamed::Tab => Key::Named(SKey::Tab),
            WNamed::Enter => Key::Named(SKey::Enter),
            WNamed::Escape => Key::Named(SKey::Escape),
            WNamed::Space => Key::Character(" ".to_string()),
            WNamed::ArrowLeft => Key::Named(SKey::ArrowLeft),
            WNamed::ArrowRight => Key::Named(SKey::ArrowRight),
            WNamed::ArrowUp => Key::Named(SKey::ArrowUp),
            WNamed::ArrowDown => Key::Named(SKey::ArrowDown),
            WNamed::PageUp => Key::Named(SKey::PageUp),
            WNamed::PageDown => Key::Named(SKey::PageDown),
            WNamed::Home => Key::Named(SKey::Home),
            WNamed::End => Key::Named(SKey::End),
            WNamed::Insert => Key::Named(SKey::Insert),
            WNamed::Delete => Key::Named(SKey::Delete),
            WNamed::F1 => Key::Named(SKey::F1),
            WNamed::F2 => Key::Named(SKey::F2),
            WNamed::F3 => Key::Named(SKey::F3),
            WNamed::F4 => Key::Named(SKey::F4),
            WNamed::F5 => Key::Named(SKey::F5),
            WNamed::F6 => Key::Named(SKey::F6),
            WNamed::F7 => Key::Named(SKey::F7),
            WNamed::F8 => Key::Named(SKey::F8),
            WNamed::F9 => Key::Named(SKey::F9),
            WNamed::F10 => Key::Named(SKey::F10),
            WNamed::F11 => Key::Named(SKey::F11),
            WNamed::F12 => Key::Named(SKey::F12),
            WNamed::Shift => Key::Named(SKey::Shift),
            WNamed::Control => Key::Named(SKey::Control),
            WNamed::Alt => Key::Named(SKey::Alt),
            WNamed::Super => Key::Named(SKey::Meta),
            _ => Key::Named(SKey::Unidentified),
        },
        _ => Key::Named(SKey::Unidentified),
    }
}

pub fn map_winit_code(winit_code: winit::keyboard::PhysicalKey) -> Code {
    use winit::keyboard::PhysicalKey as WCode;
    use winit::keyboard::KeyCode as WKeyCode;
    match winit_code {
        WCode::Code(c) => match c {
            WKeyCode::KeyA => Code::KeyA,
            WKeyCode::KeyB => Code::KeyB,
            WKeyCode::KeyC => Code::KeyC,
            WKeyCode::KeyD => Code::KeyD,
            WKeyCode::KeyE => Code::KeyE,
            WKeyCode::KeyF => Code::KeyF,
            WKeyCode::KeyG => Code::KeyG,
            WKeyCode::KeyH => Code::KeyH,
            WKeyCode::KeyI => Code::KeyI,
            WKeyCode::KeyJ => Code::KeyJ,
            WKeyCode::KeyK => Code::KeyK,
            WKeyCode::KeyL => Code::KeyL,
            WKeyCode::KeyM => Code::KeyM,
            WKeyCode::KeyN => Code::KeyN,
            WKeyCode::KeyO => Code::KeyO,
            WKeyCode::KeyP => Code::KeyP,
            WKeyCode::KeyQ => Code::KeyQ,
            WKeyCode::KeyR => Code::KeyR,
            WKeyCode::KeyS => Code::KeyS,
            WKeyCode::KeyT => Code::KeyT,
            WKeyCode::KeyU => Code::KeyU,
            WKeyCode::KeyV => Code::KeyV,
            WKeyCode::KeyW => Code::KeyW,
            WKeyCode::KeyX => Code::KeyX,
            WKeyCode::KeyY => Code::KeyY,
            WKeyCode::KeyZ => Code::KeyZ,
            WKeyCode::Digit1 => Code::Digit1,
            WKeyCode::Digit2 => Code::Digit2,
            WKeyCode::Digit3 => Code::Digit3,
            WKeyCode::Digit4 => Code::Digit4,
            WKeyCode::Digit5 => Code::Digit5,
            WKeyCode::Digit6 => Code::Digit6,
            WKeyCode::Digit7 => Code::Digit7,
            WKeyCode::Digit8 => Code::Digit8,
            WKeyCode::Digit9 => Code::Digit9,
            WKeyCode::Digit0 => Code::Digit0,
            WKeyCode::Space => Code::Space,
            WKeyCode::Enter => Code::Enter,
            WKeyCode::Escape => Code::Escape,
            WKeyCode::Backspace => Code::Backspace,
            WKeyCode::Tab => Code::Tab,
            WKeyCode::ArrowLeft => Code::ArrowLeft,
            WKeyCode::ArrowRight => Code::ArrowRight,
            WKeyCode::ArrowUp => Code::ArrowUp,
            WKeyCode::ArrowDown => Code::ArrowDown,
            WKeyCode::ShiftLeft => Code::ShiftLeft,
            WKeyCode::ShiftRight => Code::ShiftRight,
            WKeyCode::ControlLeft => Code::ControlLeft,
            WKeyCode::ControlRight => Code::ControlRight,
            WKeyCode::AltLeft => Code::AltLeft,
            WKeyCode::AltRight => Code::AltRight,
            WKeyCode::SuperLeft => Code::MetaLeft,
            WKeyCode::SuperRight => Code::MetaRight,
            _ => Code::Unidentified,
        },
        _ => Code::Unidentified,
    }
}

pub struct PaneInstance {
    pub id: String,
    pub webview: servo::WebView,
    pub rect: euclid::Rect<f32, servo::DeviceIndependentPixel>,
    pub last_notified_rect: Option<euclid::Rect<f32, servo::DeviceIndependentPixel>>,
    pub last_physical_rect: euclid::default::Rect<i32>,
    pub z_index: i32,
    pub anchor: PaneAnchor,
    pub dock_order: u32,
    pub animating: bool,
    pub is_dirty: bool,
    pub needs_repaint: bool,
    pub is_visible: bool,
    /// When true, hit-testing skips this pane so clicks fall through (crosshair, etc.).
    /// In atrium game_mode input already bypasses the stack; this matters in overlay/edit.
    pub pointer_passthrough: bool,
    pub drag_regions: Vec<euclid::Rect<f32, servo::DevicePixel>>,
    pub no_drag_regions: Vec<euclid::Rect<f32, servo::DevicePixel>>,
    pub offscreen_ctx: Rc<LotusPaneContext>,
    pub servo_busy: bool,
    pub pending_servo_size: Option<ServoPhysicalSize<u32>>,
    pub requested_servo_size: ServoPhysicalSize<u32>,
    pub current_servo_size: ServoPhysicalSize<u32>,
    pub ready_frame_size: Option<ServoPhysicalSize<u32>>,
    pub ready_to_repaint: Arc<AtomicBool>,
    pub first_frame_painted: bool,
    pub first_frame_painted_time: Option<std::time::Instant>,
    pub pending_physical_rect: Option<euclid::default::Rect<i32>>,
    pub is_resizing: bool,
    pub pending_visible: bool,
    pub ghost_tex: Option<glow::NativeTexture>,
    pub ghost_tex_size: Option<winit::dpi::PhysicalSize<u32>>,
    /// True only after a successful capture from a live-drawn FBO. Freeze/stretch
    /// must not use the ghost until this is set (avoids black quads on startup resize).
    pub ghost_valid: bool,
    /// Bumped when this pane's pixels actually change (local paint / new remote SHM).
    pub frame_seq: u64,
    /// `frame_seq` at the last host present that included this pane.
    pub last_presented_seq: u64,
    pub is_remote: bool,
    /// Linux sandbox remote pane SHM consumer (always `None` on Windows).
    pub remote_consumer: Option<crate::gl::RemotePaneConsumer>,
    /// Linux control-socket FD for remote input (`None` on Windows).
    pub remote_input_fd: Option<i32>,
    pub resize_generation: u64,
    /// Host crash chrome is covering this remote — still pull SHM/vsync, but do not
    /// draw the (possibly stale) texture into the scene.
    pub under_crash_cover: bool,
    /// After a respawn, emit `remote-frame-composited` once a fresh SHM frame is
    /// uploaded so JS can lift crash chrome without flashing the prior life.
    pub await_host_composited_frame: bool,
    #[cfg(target_os = "linux")]
    pub external_surface_slot: Option<Arc<parking_lot::Mutex<crate::wayland::ExternalSurfaceSlot>>>,
    #[cfg(target_os = "linux")]
    pub external_wayland_server: Option<Arc<parking_lot::Mutex<crate::wayland::LotusWaylandServer>>>,
    #[cfg(target_os = "linux")]
    pub external_child_pid: Option<u32>,
    #[cfg(target_os = "linux")]
    pub external_follow_dock: bool,
    #[cfg(target_os = "linux")]
    pub external_client_upscale_sgsr: bool,
    pub overlay_resize: Option<crate::overlay_resize::OverlayResizePolicy>,
}

/// Terminate an atrium external child (Proton/flatpak) and its process group.
/// SIGTERM the group first, then SIGKILL after a short grace if still alive.
#[cfg(target_os = "linux")]
pub fn terminate_external_child(pid: u32) {
    if pid == 0 {
        return;
    }
    let pid_i = pid as i32;
    unsafe {
        // Process group (spawned with setpgid) — covers wineserver helpers.
        let _ = libc::kill(-pid_i, libc::SIGTERM);
        let _ = libc::kill(pid_i, libc::SIGTERM);
    }
    for _ in 0..20 {
        let alive = unsafe { libc::kill(pid_i, 0) == 0 };
        if !alive {
            // Reap if we are parent; ignore ECHILD.
            let mut status = 0;
            unsafe {
                let _ = libc::waitpid(pid_i, &mut status, libc::WNOHANG);
            }
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    unsafe {
        let _ = libc::kill(-pid_i, libc::SIGKILL);
        let _ = libc::kill(pid_i, libc::SIGKILL);
        let mut status = 0;
        let _ = libc::waitpid(pid_i, &mut status, libc::WNOHANG);
    }
    log::info!("[lotus-nest] terminated external child pid {}", pid);
}

impl Drop for PaneInstance {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(fd) = self.remote_input_fd.take() {
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(pid) = self.external_child_pid.take() {
            terminate_external_child(pid);
        }
    }
}

impl PaneInstance {
    /// Physical hit/input rect. Prefer pending/staged layout, then last committed,
    /// then scaled logical — so tree-backed leaves (often Anchor::None) still receive input.
    pub fn input_physical_rect(&self, scale_factor: f32) -> (f32, f32, f32, f32) {
        if let Some(p) = self.pending_physical_rect {
            if p.size.width > 0 && p.size.height > 0 {
                return (
                    p.origin.x as f32,
                    p.origin.y as f32,
                    p.size.width as f32,
                    p.size.height as f32,
                );
            }
        }
        if self.last_physical_rect.size.width > 0 && self.last_physical_rect.size.height > 0 {
            return (
                self.last_physical_rect.origin.x as f32,
                self.last_physical_rect.origin.y as f32,
                self.last_physical_rect.size.width as f32,
                self.last_physical_rect.size.height as f32,
            );
        }
        let physical_rect = self.rect.scale(scale_factor, scale_factor);
        (
            physical_rect.origin.x.round(),
            physical_rect.origin.y.round(),
            physical_rect.size.width.round(),
            physical_rect.size.height.round(),
        )
    }
}

pub struct ActiveSelectPrompt {
    pub control_id: servo::EmbedderControlId,
    pub control_id_str: String,
    pub pane_id: String,
    pub previous_active_pane_id: String,
    pub initial_selected_options: Vec<usize>,
    pub prompt: servo::SelectElement,
    pub allow_multiple: bool,
}

#[derive(Debug, Clone)]
pub struct ScrollbarConfig {
    pub enabled: bool,
    pub theme: String,
    pub auto_hide: bool,
    pub width: u32,
}

pub struct WindowInstance {
    pub window_id: String,
    pub panes: HashMap<String, PaneInstance>,
    pub active_layout: StagedLayout,
    pub staged_layout: StagedLayout,
    pub active_pane_id: String,
    pub primary_pane_id: String,
    pub last_mouse_down_pane_id: Option<String>,
    pub rendering_context: Rc<WindowRenderingContext>,
    pub gl: Arc<glow::Context>,
    pub window: Arc<Window>,
    pub last_mouse_pos: Point2D<f32, servo::DevicePixel>,
    pub is_mouse_down: bool,
    pub modifiers: Modifiers,
    pub frameless: bool,
    pub transparent: bool,
    pub in_resize_border: bool,
    pub auto_resize_main: bool,
    pub corner_radius: Option<f64>,
    pub id_shifters: Vec<servo::WebView>,
    pub emitted_ready_to_show: bool,
    pub pending_stabilization: bool,
    pub stencil_program: Option<glow::Program>,
    pub stencil_vao: Option<glow::VertexArray>,
    pub u_stencil_size_loc: Option<glow::UniformLocation>,
    pub u_stencil_radius_loc: Option<glow::UniformLocation>,
    pub comp_program: Option<glow::Program>,
    /// Compose program for GL_TEXTURE_EXTERNAL_OES (Proton modifier dmabufs).
    pub comp_program_oes: Option<glow::Program>,
    /// SGSR 1.0 fragment upscale (sampler2D). Linux, opt-in.
    #[cfg(target_os = "linux")]
    pub sgsr_program: Option<glow::Program>,
    /// SGSR 1.0 fragment upscale (samplerExternalOES).
    #[cfg(target_os = "linux")]
    pub sgsr_program_oes: Option<glow::Program>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_size_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_radius_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_flip_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_texel_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_src_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_sharp_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_threshold_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_use_edge_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_oes_size_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_oes_radius_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_oes_flip_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_oes_texel_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_oes_src_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_oes_sharp_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_oes_threshold_loc: Option<glow::UniformLocation>,
    #[cfg(target_os = "linux")]
    pub u_sgsr_oes_use_edge_loc: Option<glow::UniformLocation>,
    /// Kept so SGSR/FSR1/RCAS can be linked lazily after startup.
    #[cfg(target_os = "linux")]
    pub comp_vs: Option<glow::Shader>,
    /// FSR1 EASU (sampler2D, texelFetch gather).
    #[cfg(target_os = "linux")]
    pub fsr1_easu_program: Option<glow::Program>,
    /// FSR1 EASU (samplerExternalOES, 4-tap). Optional — often missing on desktop GL.
    #[cfg(target_os = "linux")]
    pub fsr1_easu_program_oes: Option<glow::Program>,
    /// RCAS sharpen (sampler2D temp → scene).
    #[cfg(target_os = "linux")]
    pub fsr1_rcas_program: Option<glow::Program>,
    #[cfg(target_os = "linux")]
    pub fsr1_locs: crate::gl::fsr1::Fsr1Locations,
    /// Upright RGBA8 temp for upscale→RCAS (NEAREST / CLAMP_TO_EDGE).
    #[cfg(target_os = "linux")]
    pub upscale_temp_fbo: Option<glow::NativeFramebuffer>,
    #[cfg(target_os = "linux")]
    pub upscale_temp_tex: Option<glow::NativeTexture>,
    #[cfg(target_os = "linux")]
    pub upscale_temp_size: Option<(u32, u32)>,
    pub comp_fbo: Option<glow::NativeFramebuffer>,
    pub comp_tex: Option<glow::NativeTexture>,
    pub comp_vao: Option<glow::VertexArray>,
    pub comp_vbo: Option<glow::NativeBuffer>,
    pub u_comp_size_loc: Option<glow::UniformLocation>,
    pub u_comp_radius_loc: Option<glow::UniformLocation>,
    pub u_comp_flip_loc: Option<glow::UniformLocation>,
    pub u_comp_oes_size_loc: Option<glow::UniformLocation>,
    pub u_comp_oes_radius_loc: Option<glow::UniformLocation>,
    pub u_comp_oes_flip_loc: Option<glow::UniformLocation>,
    pub comp_tex_size: Option<winit::dpi::PhysicalSize<u32>>,
    pub scene_fbo: Option<glow::NativeFramebuffer>,
    pub scene_tex: Option<glow::NativeTexture>,
    pub scene_tex_size: Option<winit::dpi::PhysicalSize<u32>>,
    pub committed_layout: Option<StagedLayout>,
    pub active_window_size: winit::dpi::PhysicalSize<u32>,
    pub committed_window_size: Option<winit::dpi::PhysicalSize<u32>>,
    /// Inner size at the last swapchain present (`None` until the first present).
    pub last_presented_window_size: Option<winit::dpi::PhysicalSize<u32>>,
    pub active_select: Option<ActiveSelectPrompt>,
    pub scrollbar_config: ScrollbarConfig,
    pub trusted_origins: Vec<String>,
    /// Native monitor refresh rate in millihertz (e.g. 60000 = 60 Hz).
    pub monitor_refresh_rate_millihertz: u32,
    /// Effective rate pushed to sandboxed engines (may be throttled while unfocused).
    pub refresh_rate_millihertz: u32,
    /// Last OS focus state for this window (drives background vsync throttle).
    pub os_focused: bool,
    /// Fully occluded when the platform reports it (X11 / macOS; not Wayland).
    pub os_occluded: bool,
    /// Minimized when the platform reports it (`Window::is_minimized`; `None` on Wayland).
    pub os_minimized: bool,
    /// Inner size collapsed to 0×0 (minimize proxy on some backends).
    pub zero_size: bool,
    /// Virtual layout tree (geometry only). Leaves map to pane ids in `panes`.
    pub layout_root: LayoutNode,
    /// >0 while JS `batchLayout` is open — defer recalculate_layout until depth returns to 0.
    pub layout_batch_depth: u32,
    /// Hit-only gutters from the latest solve (physical space).
    pub gutters: Vec<HitTestGutter>,
    /// Active splitter drag (ids + start snapshot). Applied via `pending_gutter_delta` on tick.
    pub active_gutter_drag: Option<ActiveGutterDrag>,
    /// Total Δ from drag start awaiting the next redraw apply.
    pub pending_gutter_delta: Option<i32>,
    /// Currently hovered gutter key `(left_id, right_id)` for leave/cursor restore.
    pub gutter_hover: Option<(String, String)>,
    /// Floating overlay edge resize drag (host-owned, no chrome).
    pub active_overlay_resize: Option<crate::overlay_resize::ActiveOverlayResizeDrag>,
    pub overlay_resize_hover: Option<(String, crate::overlay_resize::ActiveResizeEdge)>,
    /// Last pane that received a routed MouseMove (for synthetic leave on gutter entry).
    pub hover_pane_id: Option<String>,
    /// When true, input goes directly to the embedded Wayland/X11 game pane.
    pub game_mode: bool,
    /// Window-scoped host hotkeys (PhysicalKey + mod mask). Empty = no consume.
    #[cfg(target_os = "linux")]
    pub host_hotkeys: std::collections::HashMap<String, HostHotkeyBinding>,
    /// Keys whose Pressed was consumed by a host hotkey — matching Released is also consumed.
    #[cfg(target_os = "linux")]
    pub host_hotkey_pressed: std::collections::HashSet<winit::keyboard::PhysicalKey>,
    /// Host winit cursor grab currently applied (derived from game shoes + game_mode).
    #[cfg(target_os = "linux")]
    pub host_pointer_grabbed: bool,
    /// Game-owned shoes: pointer-constraints lock/confine last requested by the primary.
    /// Snapshot survives overlay; host grab is only worn while `game_mode` is true.
    #[cfg(target_os = "linux")]
    pub game_pointer_constraint_active: bool,
    /// Game-owned shoes: last primary cursor was Hidden (Wine/X11 trap).
    #[cfg(target_os = "linux")]
    pub game_cursor_hidden: bool,
    /// Game-owned shoes: last cursor image from the primary game client.
    #[cfg(target_os = "linux")]
    pub game_cursor: Option<crate::wayland::NestedCursor>,
    /// Last nest coords on a non-primary overlay (Discord, …) for F12 leave restore
    /// when the host cursor is still parked on the game wallpaper after unlock.
    #[cfg(target_os = "linux")]
    pub last_overlay_pointer: Option<(String, f64, f64)>,
    /// Designates the primary base game pane ID.
    pub primary_game_pane: Option<String>,
    /// Last instant telemetry stats were sent to the HUD webview.
    pub last_stats_send: Option<std::time::Instant>,
    /// Host must recomposite on the next tick (e.g. nested pane show/hide).
    pub force_scene_repaint: bool,
    /// Latest nested cursor shown on the host (Discord overlay visual, etc.).
    #[cfg(target_os = "linux")]
    pub nested_cursor: Option<crate::wayland::NestedCursor>,
    /// True while the host cursor is showing a nested visual (vs Servo/HUD default).
    #[cfg(target_os = "linux")]
    pub nested_cursor_applied: bool,
    #[cfg(target_os = "linux")]
    pub nested_compositor: Option<Arc<parking_lot::Mutex<crate::wayland::LotusWaylandServer>>>,
    /// Output-scoped VRR vs fixed-VSync presentation clock.
    #[cfg(target_os = "linux")]
    pub pacing_policy: crate::pacing::PacingPolicy,
    #[cfg(target_os = "linux")]
    pub pacing_policy_generation: u64,
    /// Servo/HUD wants repaint — piggyback on next game present while GameActive (Guard B).
    #[cfg(target_os = "linux")]
    pub overlay_dirty: bool,
    /// Slow nested FrameDone while host is occluded (Guard A).
    #[cfg(target_os = "linux")]
    pub last_occluded_frame_done: Option<std::time::Instant>,
}

impl WindowInstance {
    /// Update a detached floating pane rect (overlay edge drag or JS setRect).
    pub fn apply_floating_pane_rect(
        &mut self,
        pane_id: &str,
        rect: euclid::Rect<f32, servo::DeviceIndependentPixel>,
        scale_factor: f32,
    ) {
        let Some(pane) = self.panes.get_mut(pane_id) else {
            return;
        };
        pane.rect = rect;
        pane.last_notified_rect = Some(rect);
        let pane_phys = euclid::default::Rect::new(
            euclid::default::Point2D::new(
                (rect.origin.x * scale_factor).round() as i32,
                (rect.origin.y * scale_factor).round() as i32,
            ),
            euclid::default::Size2D::new(
                (rect.size.width * scale_factor).round() as i32,
                (rect.size.height * scale_factor).round() as i32,
            ),
        );
        pane.pending_physical_rect = Some(pane_phys);
        pane.last_physical_rect = pane_phys;
        pane.anchor = crate::layout::PaneAnchor::None;
        pane.is_dirty = true;

        #[cfg(target_os = "linux")]
        if pane.external_wayland_server.is_some() {
            if let Some(ref ws) = self.nested_compositor {
                let w = ((rect.size.width * scale_factor).round() as u32).max(1);
                let h = ((rect.size.height * scale_factor).round() as u32).max(1);
                ws.lock().send_resize_pane(pane_id, w, h);
            }
        }

        self.layout_root.set_leaf_detached(pane_id, true);
        let physical_size = self.window.inner_size();
        self.recalculate_layout(physical_size, scale_factor);
        self.window.request_redraw();
    }

    /// Apply pending gutter Δ (from start snapshot) once per frame. Returns true if layout mutated.
    pub fn flush_pending_gutter_drag(&mut self, scale_factor: f32) -> bool {
        let (drag, delta) = match (
            self.active_gutter_drag.clone(),
            self.pending_gutter_delta.take(),
        ) {
            (Some(d), Some(delta)) => (d, delta),
            _ => return false,
        };
        match crate::layout::apply_gutter_drag(&mut self.layout_root, &drag, delta, scale_factor) {
            Ok(changed) => changed,
            Err(crate::layout::DragCancel::MissingParticipant) => {
                self.active_gutter_drag = None;
                self.pending_gutter_delta = None;
                self.gutter_hover = None;
                false
            }
        }
    }

    /// Escape: restore start snapshot then clear capture.
    pub fn cancel_gutter_drag_revert(&mut self, scale_factor: f32) -> bool {
        let Some(drag) = self.active_gutter_drag.take() else {
            self.pending_gutter_delta = None;
            return false;
        };
        self.pending_gutter_delta = None;
        self.gutter_hover = None;
        let _ = crate::layout::apply_gutter_drag(&mut self.layout_root, &drag, 0, scale_factor);
        true
    }

    /// Blur / CursorLeft / structural: keep current geometry, drop capture.
    pub fn cancel_gutter_drag_keep(&mut self) {
        self.active_gutter_drag = None;
        self.pending_gutter_delta = None;
        self.gutter_hover = None;
    }

    pub fn init_stencil_program(&mut self) {
        use glow::HasContext;
        unsafe {
            let program = self.gl.create_program().expect("Cannot create program");

            let vs = self.gl.create_shader(glow::VERTEX_SHADER).expect("Cannot create shader");
            self.gl.shader_source(vs, STENCIL_VS);
            self.gl.compile_shader(vs);
            if !self.gl.get_shader_compile_status(vs) {
                panic!("Stencil VS compile error: {}", self.gl.get_shader_info_log(vs));
            }

            let fs = self.gl.create_shader(glow::FRAGMENT_SHADER).expect("Cannot create shader");
            self.gl.shader_source(fs, STENCIL_FS);
            self.gl.compile_shader(fs);
            if !self.gl.get_shader_compile_status(fs) {
                panic!("Stencil FS compile error: {}", self.gl.get_shader_info_log(fs));
            }

            self.gl.attach_shader(program, vs);
            self.gl.attach_shader(program, fs);
            self.gl.link_program(program);
            if !self.gl.get_program_link_status(program) {
                panic!("Stencil program link error: {}", self.gl.get_program_info_log(program));
            }

            self.gl.detach_shader(program, vs);
            self.gl.delete_shader(vs);
            self.gl.detach_shader(program, fs);
            self.gl.delete_shader(fs);

            self.u_stencil_size_loc = self.gl.get_uniform_location(program, "u_size");
            self.u_stencil_radius_loc = self.gl.get_uniform_location(program, "u_radius");
            self.stencil_program = Some(program);

            let vao = self.gl.create_vertex_array().expect("Cannot create VAO");
            self.stencil_vao = Some(vao);
        }
    }

    #[cfg(target_os = "linux")]
    fn compile_fsr1_programs(&mut self, vs: glow::Shader) {
        // Eager path kept for callers that already hold VS; prefer ensure_* lazy API.
        self.fsr1_easu_program = self.link_fsr1_easu(vs, false);
        self.fsr1_easu_program_oes = self.link_fsr1_easu(vs, true);
        self.fsr1_rcas_program = self.link_fsr1_rcas(vs);
    }

    #[cfg(target_os = "linux")]
    fn link_fsr1_easu(&mut self, vs: glow::Shader, external: bool) -> Option<glow::Program> {
        use glow::HasContext;
        unsafe {
            let program = self.gl.create_program().ok()?;
            let fs = self.gl.create_shader(glow::FRAGMENT_SHADER).ok()?;
            let src = if external {
                crate::gl::fsr1::easu_fragment_oes()
            } else {
                crate::gl::fsr1::easu_fragment_2d()
            };
            self.gl.shader_source(fs, &src);
            self.gl.compile_shader(fs);
            if !self.gl.get_shader_compile_status(fs) {
                log::warn!(
                    "[lotus-fsr1] EASU compile failed (external={external}): {}",
                    self.gl.get_shader_info_log(fs)
                );
                self.gl.delete_shader(fs);
                self.gl.delete_program(program);
                return None;
            }
            self.gl.attach_shader(program, vs);
            self.gl.attach_shader(program, fs);
            self.gl.link_program(program);
            self.gl.detach_shader(program, vs);
            self.gl.detach_shader(program, fs);
            self.gl.delete_shader(fs);
            if !self.gl.get_program_link_status(program) {
                log::warn!(
                    "[lotus-fsr1] EASU link failed (external={external}): {}",
                    self.gl.get_program_info_log(program)
                );
                self.gl.delete_program(program);
                return None;
            }
            if let Some(u_tex) = self.gl.get_uniform_location(program, "u_texture") {
                self.gl.use_program(Some(program));
                self.gl.uniform_1_i32(Some(&u_tex), 0);
                self.gl.use_program(None);
            }
            let locs = &mut self.fsr1_locs;
            if external {
                locs.easu_oes_size = self.gl.get_uniform_location(program, "u_size");
                locs.easu_oes_radius = self.gl.get_uniform_location(program, "u_radius");
                locs.easu_oes_flip = self.gl.get_uniform_location(program, "u_flip_y");
                locs.easu_oes_origin = self.gl.get_uniform_location(program, "u_origin");
                locs.easu_oes_src = self.gl.get_uniform_location(program, "u_src_size");
                locs.easu_oes_texel = self.gl.get_uniform_location(program, "u_texel_size");
                locs.easu_oes_con0 = self.gl.get_uniform_location(program, "u_con0");
                locs.easu_oes_con1 = self.gl.get_uniform_location(program, "u_con1");
                locs.easu_oes_con2 = self.gl.get_uniform_location(program, "u_con2");
                locs.easu_oes_con3 = self.gl.get_uniform_location(program, "u_con3");
            } else {
                locs.easu_size = self.gl.get_uniform_location(program, "u_size");
                locs.easu_radius = self.gl.get_uniform_location(program, "u_radius");
                locs.easu_flip = self.gl.get_uniform_location(program, "u_flip_y");
                locs.easu_origin = self.gl.get_uniform_location(program, "u_origin");
                locs.easu_src = self.gl.get_uniform_location(program, "u_src_size");
                locs.easu_texel = self.gl.get_uniform_location(program, "u_texel_size");
                locs.easu_con0 = self.gl.get_uniform_location(program, "u_con0");
                locs.easu_con1 = self.gl.get_uniform_location(program, "u_con1");
                locs.easu_con2 = self.gl.get_uniform_location(program, "u_con2");
                locs.easu_con3 = self.gl.get_uniform_location(program, "u_con3");
            }
            Some(program)
        }
    }

    #[cfg(target_os = "linux")]
    fn link_fsr1_rcas(&mut self, vs: glow::Shader) -> Option<glow::Program> {
        use glow::HasContext;
        unsafe {
            let program = self.gl.create_program().ok()?;
            let fs = self.gl.create_shader(glow::FRAGMENT_SHADER).ok()?;
            self.gl
                .shader_source(fs, &crate::gl::fsr1::rcas_fragment());
            self.gl.compile_shader(fs);
            if !self.gl.get_shader_compile_status(fs) {
                log::warn!(
                    "[lotus-fsr1] RCAS compile failed: {}",
                    self.gl.get_shader_info_log(fs)
                );
                self.gl.delete_shader(fs);
                self.gl.delete_program(program);
                return None;
            }
            self.gl.attach_shader(program, vs);
            self.gl.attach_shader(program, fs);
            self.gl.link_program(program);
            self.gl.detach_shader(program, vs);
            self.gl.detach_shader(program, fs);
            self.gl.delete_shader(fs);
            if !self.gl.get_program_link_status(program) {
                log::warn!(
                    "[lotus-fsr1] RCAS link failed: {}",
                    self.gl.get_program_info_log(program)
                );
                self.gl.delete_program(program);
                return None;
            }
            if let Some(u_tex) = self.gl.get_uniform_location(program, "u_texture") {
                self.gl.use_program(Some(program));
                self.gl.uniform_1_i32(Some(&u_tex), 0);
                self.gl.use_program(None);
            }
            self.fsr1_locs.rcas_size = self.gl.get_uniform_location(program, "u_size");
            self.fsr1_locs.rcas_radius = self.gl.get_uniform_location(program, "u_radius");
            self.fsr1_locs.rcas_origin = self.gl.get_uniform_location(program, "u_origin");
            self.fsr1_locs.rcas_con = self.gl.get_uniform_location(program, "u_con");
            Some(program)
        }
    }

    #[cfg(target_os = "linux")]
    fn compile_sgsr_programs(&mut self, vs: glow::Shader) {
        self.sgsr_program = self.link_sgsr_variant(vs, false);
        self.sgsr_program_oes = self.link_sgsr_variant(vs, true);
    }

    #[cfg(target_os = "linux")]
    fn link_sgsr_variant(&mut self, vs: glow::Shader, external: bool) -> Option<glow::Program> {
        use glow::HasContext;
        unsafe {
            let program = self.gl.create_program().ok()?;
            let fs = self.gl.create_shader(glow::FRAGMENT_SHADER).ok()?;
            self.gl
                .shader_source(fs, &sgsr_fragment_source(external));
            self.gl.compile_shader(fs);
            if !self.gl.get_shader_compile_status(fs) {
                log::warn!(
                    "[lotus-sgsr] compile failed (external={external}): {}",
                    self.gl.get_shader_info_log(fs)
                );
                self.gl.delete_shader(fs);
                self.gl.delete_program(program);
                return None;
            }
            self.gl.attach_shader(program, vs);
            self.gl.attach_shader(program, fs);
            self.gl.link_program(program);
            self.gl.detach_shader(program, vs);
            self.gl.detach_shader(program, fs);
            self.gl.delete_shader(fs);
            if !self.gl.get_program_link_status(program) {
                log::warn!(
                    "[lotus-sgsr] link failed (external={external}): {}",
                    self.gl.get_program_info_log(program)
                );
                self.gl.delete_program(program);
                return None;
            }
            let size = self.gl.get_uniform_location(program, "u_size");
            let radius = self.gl.get_uniform_location(program, "u_radius");
            let flip = self.gl.get_uniform_location(program, "u_flip_y");
            let texel = self.gl.get_uniform_location(program, "u_texel_size");
            let src = self.gl.get_uniform_location(program, "u_src_size");
            let sharp = self.gl.get_uniform_location(program, "u_edge_sharpness");
            let threshold = self.gl.get_uniform_location(program, "u_edge_threshold");
            let use_edge = self.gl.get_uniform_location(program, "u_use_edge_direction");
            if let Some(u_tex) = self.gl.get_uniform_location(program, "u_texture") {
                let sharp_v = sgsr_edge_sharpness();
                let thresh_v = sgsr_edge_threshold();
                let use_edge_v = if sgsr_use_edge_direction() { 1 } else { 0 };
                self.gl.use_program(Some(program));
                self.gl.uniform_1_i32(Some(&u_tex), 0);
                self.gl.uniform_1_f32(sharp.as_ref(), sharp_v);
                self.gl.uniform_1_f32(threshold.as_ref(), thresh_v);
                self.gl.uniform_1_i32(use_edge.as_ref(), use_edge_v);
                self.gl.use_program(None);
            }
            if external {
                self.u_sgsr_oes_size_loc = size;
                self.u_sgsr_oes_radius_loc = radius;
                self.u_sgsr_oes_flip_loc = flip;
                self.u_sgsr_oes_texel_loc = texel;
                self.u_sgsr_oes_src_loc = src;
                self.u_sgsr_oes_sharp_loc = sharp;
                self.u_sgsr_oes_threshold_loc = threshold;
                self.u_sgsr_oes_use_edge_loc = use_edge;
            } else {
                self.u_sgsr_size_loc = size;
                self.u_sgsr_radius_loc = radius;
                self.u_sgsr_flip_loc = flip;
                self.u_sgsr_texel_loc = texel;
                self.u_sgsr_src_loc = src;
                self.u_sgsr_sharp_loc = sharp;
                self.u_sgsr_threshold_loc = threshold;
                self.u_sgsr_use_edge_loc = use_edge;
            }
            Some(program)
        }
    }

    /// Compile SGSR programs on demand (GL thread). Safe to call repeatedly.
    ///
    /// `sampler2D` is required (DMA-BUF imports prefer TEXTURE_2D). OES is
    /// optional — desktop GL often rejects `GL_OES_EGL_image_external` in
    /// `#version 330`, and we only try it once.
    #[cfg(target_os = "linux")]
    pub fn ensure_sgsr_programs(&mut self) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static OES_TRIED: AtomicBool = AtomicBool::new(false);

        if self.sgsr_program.is_some() {
            if self.sgsr_program_oes.is_none() && !OES_TRIED.swap(true, Ordering::Relaxed) {
                if let Some(vs) = self.comp_vs {
                    self.sgsr_program_oes = self.link_sgsr_variant(vs, true);
                    if self.sgsr_program_oes.is_none() {
                        log::warn!(
                            "[lotus-sgsr] OES variant unavailable (ok — DMA-BUF uses sampler2D)"
                        );
                    }
                }
            }
            return;
        }
        let Some(vs) = self.comp_vs else {
            log::warn!("[lotus-sgsr] ensure skipped: composition VS not kept");
            return;
        };
        self.sgsr_program = self.link_sgsr_variant(vs, false);
        if !OES_TRIED.swap(true, Ordering::Relaxed) {
            self.sgsr_program_oes = self.link_sgsr_variant(vs, true);
            if self.sgsr_program_oes.is_none() {
                log::warn!("[lotus-sgsr] OES variant unavailable (ok — DMA-BUF uses sampler2D)");
            }
        }
        if self.sgsr_program.is_some() {
            log::info!(
                "[lotus-sgsr] lazy link 2d=true oes={} sharpness={:.3} threshold={:.5} edge_dir={}",
                self.sgsr_program_oes.is_some(),
                sgsr_edge_sharpness(),
                sgsr_edge_threshold(),
                sgsr_use_edge_direction()
            );
        } else {
            log::error!("[lotus-sgsr] lazy link FAILED (2d) — draws will use bilinear");
        }
    }

    /// Compile FSR1 EASU + RCAS on demand (GL thread).
    ///
    /// `sampler2D` EASU is required. OES is optional and tried once — missing
    /// OES is not a bilinear fallback when the 2D program linked.
    #[cfg(target_os = "linux")]
    pub fn ensure_fsr1_programs(&mut self) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static OES_TRIED: AtomicBool = AtomicBool::new(false);

        if self.fsr1_easu_program.is_some() && self.fsr1_rcas_program.is_some() {
            if self.fsr1_easu_program_oes.is_none() && !OES_TRIED.swap(true, Ordering::Relaxed) {
                if let Some(vs) = self.comp_vs {
                    self.fsr1_easu_program_oes = self.link_fsr1_easu(vs, true);
                    if self.fsr1_easu_program_oes.is_none() {
                        log::warn!(
                            "[lotus-fsr1] EASU OES unavailable (ok — DMA-BUF uses sampler2D)"
                        );
                    }
                }
            }
            return;
        }
        let Some(vs) = self.comp_vs else {
            log::warn!("[lotus-fsr1] ensure skipped: composition VS not kept");
            return;
        };
        if self.fsr1_easu_program.is_none() {
            self.fsr1_easu_program = self.link_fsr1_easu(vs, false);
        }
        if self.fsr1_easu_program_oes.is_none() && !OES_TRIED.swap(true, Ordering::Relaxed) {
            self.fsr1_easu_program_oes = self.link_fsr1_easu(vs, true);
            if self.fsr1_easu_program_oes.is_none() {
                log::warn!("[lotus-fsr1] EASU OES unavailable (ok — DMA-BUF uses sampler2D)");
            }
        }
        if self.fsr1_rcas_program.is_none() {
            self.fsr1_rcas_program = self.link_fsr1_rcas(vs);
        }
        let ok2d = self.fsr1_easu_program.is_some();
        let ok_rcas = self.fsr1_rcas_program.is_some();
        if ok2d {
            log::info!(
                "[lotus-fsr1] lazy link easu_2d=true easu_oes={} rcas={} stops={:.3}",
                self.fsr1_easu_program_oes.is_some(),
                ok_rcas,
                rcas_sharpness_stops()
            );
        } else {
            log::error!(
                "[lotus-fsr1] lazy link FAILED easu_2d=false rcas={} — FSR draws will use bilinear",
                ok_rcas
            );
        }
    }

    /// Ensure only the shaders required by the current host upscale config.
    #[cfg(target_os = "linux")]
    pub fn ensure_host_upscale_shaders(&mut self) {
        let cfg = host_upscale_config();
        match cfg.mode {
            HostUpscale::Sgsr => self.ensure_sgsr_programs(),
            HostUpscale::Fsr1 => self.ensure_fsr1_programs(),
            HostUpscale::Off => {}
        }
        if cfg.sharpen_rcas {
            // RCAS lives with FSR1 program set.
            self.ensure_fsr1_programs();
        }
    }

    pub fn init_composition_resources(&mut self) {
        use glow::HasContext;
        unsafe {
            let program = self.gl.create_program().expect("Cannot create program");

            let vs = self.gl.create_shader(glow::VERTEX_SHADER).expect("Cannot create shader");
            self.gl.shader_source(vs, COMP_VS);
            self.gl.compile_shader(vs);
            if !self.gl.get_shader_compile_status(vs) {
                panic!("Comp VS compile error: {}", self.gl.get_shader_info_log(vs));
            }

            let fs = self.gl.create_shader(glow::FRAGMENT_SHADER).expect("Cannot create shader");
            self.gl.shader_source(fs, COMP_FS);
            self.gl.compile_shader(fs);
            if !self.gl.get_shader_compile_status(fs) {
                panic!("Comp FS compile error: {}", self.gl.get_shader_info_log(fs));
            }

            self.gl.attach_shader(program, vs);
            self.gl.attach_shader(program, fs);
            self.gl.link_program(program);
            if !self.gl.get_program_link_status(program) {
                panic!("Comp program link error: {}", self.gl.get_program_info_log(program));
            }

            self.gl.detach_shader(program, vs);
            // Keep VS for OES program link below; delete after both programs link.
            self.gl.detach_shader(program, fs);
            self.gl.delete_shader(fs);

            self.u_comp_size_loc = self.gl.get_uniform_location(program, "u_size");
            self.u_comp_radius_loc = self.gl.get_uniform_location(program, "u_radius");
            self.u_comp_flip_loc = self.gl.get_uniform_location(program, "u_flip_y");
            self.comp_program = Some(program);

            // Optional OES external program — Linux nest DMA-BUF path only.
            #[cfg(target_os = "linux")]
            {
                let oes_program = self.gl.create_program().expect("Cannot create OES program");
                let fs_oes = self
                    .gl
                    .create_shader(glow::FRAGMENT_SHADER)
                    .expect("Cannot create OES FS");
                self.gl.shader_source(fs_oes, COMP_FS_OES);
                self.gl.compile_shader(fs_oes);
                if !self.gl.get_shader_compile_status(fs_oes) {
                    log::warn!(
                        "[lotus-nest] COMP_FS_OES compile failed (external dmabufs may stay black): {}",
                        self.gl.get_shader_info_log(fs_oes)
                    );
                    self.gl.delete_shader(fs_oes);
                    self.gl.delete_program(oes_program);
                    self.comp_program_oes = None;
                } else {
                    self.gl.attach_shader(oes_program, vs);
                    self.gl.attach_shader(oes_program, fs_oes);
                    self.gl.link_program(oes_program);
                    if !self.gl.get_program_link_status(oes_program) {
                        log::warn!(
                            "[lotus-nest] COMP_FS_OES link failed: {}",
                            self.gl.get_program_info_log(oes_program)
                        );
                        self.gl.detach_shader(oes_program, vs);
                        self.gl.detach_shader(oes_program, fs_oes);
                        self.gl.delete_shader(fs_oes);
                        self.gl.delete_program(oes_program);
                        self.comp_program_oes = None;
                    } else {
                        self.gl.detach_shader(oes_program, vs);
                        self.gl.detach_shader(oes_program, fs_oes);
                        self.gl.delete_shader(fs_oes);
                        self.u_comp_oes_size_loc =
                            self.gl.get_uniform_location(oes_program, "u_size");
                        self.u_comp_oes_radius_loc =
                            self.gl.get_uniform_location(oes_program, "u_radius");
                        self.u_comp_oes_flip_loc =
                            self.gl.get_uniform_location(oes_program, "u_flip_y");
                        if let Some(u_tex) = self.gl.get_uniform_location(oes_program, "u_texture")
                        {
                            self.gl.use_program(Some(oes_program));
                            self.gl.uniform_1_i32(Some(&u_tex), 0);
                            self.gl.use_program(None);
                        }
                        self.comp_program_oes = Some(oes_program);
                        log::info!("[lotus-nest] COMP_FS_OES ready (samplerExternalOES)");
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                self.comp_program_oes = None;
            }

            // Keep VS for lazy SGSR/FSR1/RCAS link when the UI enables a mode.
            #[cfg(target_os = "linux")]
            {
                self.comp_vs = Some(vs);
                self.ensure_host_upscale_shaders();
            }
            #[cfg(not(target_os = "linux"))]
            self.gl.delete_shader(vs);

            let vao = self.gl.create_vertex_array().expect("Cannot create VAO");
            self.gl.bind_vertex_array(Some(vao));

            let vbo = self.gl.create_buffer().expect("Cannot create VBO");
            self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));

            let quad_verts: [f32; 8] = [
                -1.0, -1.0,
                 1.0, -1.0,
                -1.0,  1.0,
                 1.0,  1.0,
            ];
            self.gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck::cast_slice(&quad_verts), glow::STATIC_DRAW);

            self.gl.enable_vertex_attrib_array(0);
            self.gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);

            self.gl.bind_vertex_array(None);
            self.comp_vao = Some(vao);
            self.comp_vbo = Some(vbo);

            // Create FBO and Texture
            let fbo = self.gl.create_framebuffer().expect("Cannot create FBO");
            let tex = self.gl.create_texture().expect("Cannot create Texture");

            self.comp_fbo = Some(fbo);
            self.comp_tex = Some(tex);
        }
    }

    pub fn recalculate_layout(&mut self, size: winit::dpi::PhysicalSize<u32>, scale_factor: f32) {
        let window_rect = euclid::Rect::new(
            euclid::default::Point2D::new(0, 0),
            euclid::default::Size2D::new(size.width as i32, size.height as i32),
        );

        let mut next_staged = StagedLayout {
            panes: HashMap::new(),
            width: size.width,
            height: size.height,
            scale_factor,
        };
        let mut sorted_ids: Vec<_> = self.panes.keys().cloned().collect();
        sorted_ids.sort_by_key(|id| self.panes.get(id).map(|p| p.dock_order).unwrap_or(0));

        for id in &sorted_ids {
            let pane = self.panes.get(id).unwrap();
            next_staged.panes.insert(id.clone(), StagedPaneLayout {
                rect: pane.last_physical_rect,
                visible: pane.pending_visible,
            });
        }

        // Tree solve (physical space) → leaf rects + hit-only gutters.
        let solved_out = crate::layout::solve_layout(&self.layout_root, window_rect, scale_factor);
        let solved = solved_out.rects;
        self.gutters = solved_out.gutters;
        // Cancel drag if either participant vanished / stopped participating.
        if let Some(ref drag) = self.active_gutter_drag {
            let left_ok = drag.left_id == crate::layout::DOCK_INNER_ID
                || self.layout_root.child_participating(&drag.left_id);
            let right_ok = drag.right_id == crate::layout::DOCK_INNER_ID
                || self.layout_root.child_participating(&drag.right_id);
            if !left_ok || !right_ok {
                self.active_gutter_drag = None;
                self.pending_gutter_delta = None;
                self.gutter_hover = None;
            }
        }

        // Crash-chrome mirrors covered leaf rect every tick.
        let mut chrome_mirrors: Vec<(String, euclid::default::Rect<i32>)> = Vec::new();
        for (id, pane) in self.panes.iter() {
            if pane.under_crash_cover {
                if let Some(leaf_rect) = solved.get(id) {
                    let crash_id = format!("__lotus_crash_{}", id);
                    chrome_mirrors.push((crash_id, *leaf_rect));
                }
            }
        }

        for id in sorted_ids {
            let pane = self.panes.get_mut(&id).unwrap();

            #[cfg(target_os = "linux")]
            let keep_hidden_external = pane.external_wayland_server.is_some();
            #[cfg(not(target_os = "linux"))]
            let keep_hidden_external = false;

            let pane_phys = if let Some(rect) = solved.get(&id) {
                if !pane.pending_visible && !keep_hidden_external {
                    continue;
                }
                *rect
            } else if pane.anchor == PaneAnchor::None {
                // Nested Wayland overlays still need geometry + xdg configures while
                // hidden so Discord/Electron adopt the dock size before first show.
                if !pane.pending_visible && !keep_hidden_external {
                    continue;
                }
                // Absolute / overlay (incl. crash chrome before mirror apply)
                if let Some((_, mirrored)) = chrome_mirrors.iter().find(|(cid, _)| cid == &id) {
                    *mirrored
                } else {
                    let phys_x = (pane.rect.origin.x * scale_factor).round() as i32;
                    let phys_y = (pane.rect.origin.y * scale_factor).round() as i32;
                    let phys_w = (pane.rect.size.width * scale_factor).round() as i32;
                    let phys_h = (pane.rect.size.height * scale_factor).round() as i32;
                    euclid::Rect::new(
                        euclid::default::Point2D::new(phys_x, phys_y),
                        euclid::default::Size2D::new(phys_w, phys_h),
                    )
                }
            } else {
                // Docked pane not in tree solve output (hidden/detached) — skip
                continue;
            };

            #[cfg(target_os = "linux")]
            if pane.external_wayland_server.is_some() {
                // Same-window: snap last_physical so composite matches staged immediately.
                // Window-size change: leave last_physical for nuclear flip — scene FBO is
                // still active_window_size; writing new rects here draws them into the old
                // FBO and stretch-blits wrong until a later resize unsticks the flip
                // (maximize → image stuck until drag-resize).
                if size == self.active_window_size {
                    pane.last_physical_rect = pane_phys;
                }
                if let Some(staged) = next_staged.panes.get_mut(&id) {
                    staged.rect = pane_phys;
                    staged.visible = pane.pending_visible;
                }
                let target_w = (pane_phys.size.width as u32).max(1);
                let target_h = (pane_phys.size.height as u32).max(1);
                if let Some(ref ws) = self.nested_compositor {
                    ws.lock().send_resize_pane(&id, target_w, target_h);
                }
                continue;
            }

            if let Some(staged) = next_staged.panes.get_mut(&id) {
                staged.rect = pane_phys;
                staged.visible = pane.pending_visible;
            }

            let servo_size = ServoPhysicalSize::new(
                (pane_phys.size.width as u32).max(1),
                (pane_phys.size.height as u32).max(1)
            );
            let logical_rect = euclid::Rect::new(
                euclid::Point2D::new(pane_phys.origin.x as f32 / scale_factor, pane_phys.origin.y as f32 / scale_factor),
                euclid::Size2D::new(pane_phys.size.width as f32 / scale_factor, pane_phys.size.height as f32 / scale_factor)
            );

            // Always sync logical rect from the tree solve (incl. Anchor::None split leaves).
            pane.rect = logical_rect;

            // Same-window: arm pending for *every* solved pane every tick so input +
            // composite never mix last_physical on one neighbor with pending on another
            // (that mismatch is the cross-pane black bar when resizing 1|2 affects 3).
            // Window-size changes must not — staged is ahead of active_window_size.
            if size == self.active_window_size {
                pane.pending_physical_rect = Some(pane_phys);
            } else {
                pane.pending_physical_rect = None;
            }

            if pane.last_notified_rect != Some(logical_rect) || pane.is_dirty {
                let size_changed =
                    servo_size != pane.current_servo_size || !pane.first_frame_painted;
                // Only real geometry changes enter resize-freeze. Visibility toggles
                // set is_dirty without a size change — treating that as resize left
                // is_resizing stuck true and the compositor sampling a freeze ghost
                // until the next window resize (HUD telemetry freeze on hide/show).
                if size_changed {
                    pane.is_resizing = true;
                    pane.pending_servo_size = Some(servo_size);
                }
                pane.last_notified_rect = Some(logical_rect);
                pane.is_dirty = false;
            } else if pane.last_physical_rect != pane_phys {
                pane.is_resizing = true;
            }
        }
        self.staged_layout = next_staged;

        // Seed local ghosts once on the edge into resize *before* webview.resize()
        // updates the offscreen FBO (after last live draw, before FB realloc/clear).
        // Skip if ghost_valid — already seeded this freeze (cleared when is_resizing ends).
        let local_seed_ids: Vec<String> = self
            .panes
            .iter()
            .filter(|(_, p)| {
                !p.is_remote
                    && p.is_resizing
                    && !p.ghost_valid
                    && p.first_frame_painted
                    && p.offscreen_ctx.size().width > 0
                    && p.offscreen_ctx.size().height > 0
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in local_seed_ids {
            crate::gl::compositor::capture_local_ghost(self, &id);
        }

        // Evaluate if we are safe to update or establish a new layout transaction
        let can_commit = match &self.committed_layout {
            None => true,
            Some(_) => {
                // If Servo is not actively busy rendering any pane for the current lock,
                // it is safe to absorb rapid sequential IPC commands into the same transaction.
                !self.panes.values().filter(|p| {
                    #[cfg(target_os = "linux")]
                    if p.external_wayland_server.is_some() {
                        return false;
                    }
                    true
                }).any(|p| p.servo_busy)
            }
        };

        if can_commit {
            let needs_transaction = self.active_window_size != size
                || self.panes.values().any(|p| {
                    if let Some(staged) = self.staged_layout.panes.get(&p.id) {
                        if let Some(active) = self.active_layout.panes.get(&p.id) {
                            return staged.rect != active.rect || staged.visible != active.visible;
                        }
                        return true;
                    }
                    false
                });

            if needs_transaction {
                self.committed_layout = Some(self.staged_layout.clone());
                self.committed_window_size = Some(size);
            }
        }

        // --- LOCK-STEP DISPATCHER ---
        // Forces Servo / remote engines to render exactly what the locked transaction
        // is waiting for. Remotes must take the same queue-of-1 path as locals:
        // one in-flight Resize (servo_busy) until a matching SHM frame arrives and
        // nuclear flip commits. Dispatching Resize early on every layout tick is what
        // desynced sandbox WebGL/broker sizes on scale-up.
        //
        // Locals: never webview.resize() while a frame is waiting to paint — resize()
        // updates the Webrender document view + FB immediately.
        for pane in self.panes.values_mut() {
            #[cfg(target_os = "linux")]
            if pane.external_wayland_server.is_some() {
                continue;
            }
            let can_dispatch = if pane.is_remote {
                !pane.servo_busy
            } else {
                !pane.servo_busy && !pane.needs_repaint
            };
            if !can_dispatch {
                continue;
            }
            if let Some(committed) = &self.committed_layout {
                if let Some(staged) = committed.panes.get(&pane.id) {
                    let target_size = ServoPhysicalSize::new(
                        (staged.rect.size.width as u32).max(1),
                        (staged.rect.size.height as u32).max(1)
                    );
                    if pane.current_servo_size != target_size || !pane.first_frame_painted {
                        if pane.is_remote {
                            #[cfg(target_os = "linux")]
                            if let Some(fd) = pane.remote_input_fd {
                                if target_size.width > 0 && target_size.height > 0 {
                                    pane.resize_generation += 1;
                                    let gen = pane.resize_generation;
                                    let surface_id = format!("{}:{}", self.window_id, pane.id);
                                    let needs_realloc = pane
                                        .remote_consumer
                                        .as_ref()
                                        .map(|c| {
                                            target_size.width > c.max_width
                                                || target_size.height > c.max_height
                                        })
                                        .unwrap_or(false);

                                    if needs_realloc {
                                        // Sealed memfd cannot grow — stepped realloc + dual-map handoff.
                                        if let Err(e) = Self::realloc_remote_pixel_plane(
                                            pane,
                                            &surface_id,
                                            fd,
                                            gen,
                                            target_size.width,
                                            target_size.height,
                                            scale_factor,
                                        ) {
                                            log::warn!(
                                                "[shm] ReplacePixelPlane failed for {}: {}",
                                                surface_id,
                                                e
                                            );
                                        }
                                    } else if let Some(consumer) = &mut pane.remote_consumer {
                                        consumer.update_active_resize_gen(gen);
                                        let resize_msg = serde_json::json!({
                                            "type": "Resize",
                                            "surface_id": surface_id,
                                            "generation": gen,
                                            "width": target_size.width,
                                            "height": target_size.height,
                                            "scale_factor": scale_factor,
                                        })
                                        .to_string();
                                        crate::app::forward_remote_input(fd, &resize_msg);
                                    }
                                    pane.requested_servo_size = target_size;
                                    pane.servo_busy = true;
                                    pane.pending_servo_size = None;
                                    pane.is_resizing = true;
                                    // Drop stale readiness — flip waits for a frame at target_size.
                                    pane.ready_frame_size = None;
                                }
                            }
                        } else {
                            pane.webview.resize(target_size);
                            pane.requested_servo_size = target_size;
                            pane.servo_busy = true;
                            pane.pending_servo_size = None;
                            pane.is_resizing = true;
                            // Match remotes: drop stale readiness so flip waits for a
                            // frame at target_size. Re-arm the notify gate — a prior
                            // notify may have left it false, which would drop the
                            // Resize-driven notify_new_frame_ready and deadlock the
                            // nuclear flip (frozen ghost + black dock slot).
                            pane.ready_frame_size = None;
                            pane.ready_to_repaint.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
        }

        if self.committed_layout.is_some() {
            self.window.request_redraw();
        }
    }

    /// Grow a sealed remote pixel plane (never shrink during interactive resize).
    /// Steps dimensions to 512px multiples after 1.5× headroom, dual-maps the
    /// consumer, and sends `ReplacePixelPlane` + the new RW fd to the engine.
    #[cfg(target_os = "linux")]
    fn realloc_remote_pixel_plane(
        pane: &mut PaneInstance,
        surface_id: &str,
        control_fd: i32,
        generation: u64,
        target_w: u32,
        target_h: u32,
        scale_factor: f32,
    ) -> Result<(), String> {
        fn step_plane_dim(phys: u32) -> u32 {
            const STEP: u32 = 512;
            const MAX: u32 = 4096;
            let grown = phys.saturating_mul(3).div_ceil(2); // 1.5×
            let stepped = grown.div_ceil(STEP).saturating_mul(STEP).max(STEP);
            stepped.min(MAX)
        }

        let consumer = pane
            .remote_consumer
            .as_mut()
            .ok_or_else(|| "no remote consumer".to_string())?;
        let new_max_w = step_plane_dim(target_w).max(consumer.max_width);
        let new_max_h = step_plane_dim(target_h).max(consumer.max_height);
        if new_max_w <= consumer.max_width && new_max_h <= consumer.max_height {
            consumer.update_active_resize_gen(generation);
            let resize_msg = serde_json::json!({
                "type": "Resize",
                "surface_id": surface_id,
                "generation": generation,
                "width": target_w,
                "height": target_h,
                "scale_factor": scale_factor,
            })
            .to_string();
            crate::app::forward_remote_input(control_fd, &resize_msg);
            return Ok(());
        }

        let geom = lotus_supervisor::shm::calculate_shm_geometry(new_max_w, new_max_h)
            .map_err(|e| format!("geometry: {e}"))?;
        let total_size = 256 + geom.total_pixel_bytes;
        let plane = lotus_supervisor::shm::SealedMemfdPlane::new(
            &format!("{surface_id}_pixel_realloc"),
            total_size,
        )
        .map_err(|e| format!("memfd: {e}"))?;

        // Zero header on the new plane before the engine maps it.
        unsafe {
            let hdr =
                &mut *(plane.rw_ptr as *mut lotus_supervisor::shm::ProducerControlHeader);
            hdr.pub_seq
                .store(0, std::sync::atomic::Ordering::Relaxed);
            hdr.prod_resize_gen
                .store(generation, std::sync::atomic::Ordering::Release);
            hdr.active_slots = 3;
            for slot in &hdr.slots {
                slot.state.store(
                    lotus_supervisor::shm::STATE_FREE,
                    std::sync::atomic::Ordering::Release,
                );
                slot.pub_seq
                    .store(0, std::sync::atomic::Ordering::Release);
            }
        }

        let pixel_rw = unsafe { libc::dup(plane.rw_fd) };
        let pixel_ro = unsafe { libc::dup(plane.ro_fd) };
        if pixel_rw < 0 || pixel_ro < 0 {
            return Err("dup failed".into());
        }
        // SealedMemfdPlane Drop closes originals; dups survive.

        consumer
            .begin_plane_handoff(pixel_ro, total_size, new_max_w, new_max_h)
            .map_err(|e| e.to_string())?;
        consumer.update_active_resize_gen(generation);

        let msg = serde_json::json!({
            "type": "ReplacePixelPlane",
            "surface_id": surface_id,
            "generation": generation,
            "width": target_w,
            "height": target_h,
            "max_width": new_max_w,
            "max_height": new_max_h,
            "scale_factor": scale_factor,
        })
        .to_string();
        crate::send_fds_over_socket(control_fd, vec![pixel_rw], msg)
            .map_err(|e| e.to_string())?;
        // Engine owns the RW end after sendmsg; close our send copy.
        unsafe {
            libc::close(pixel_rw);
        }
        log::info!(
            "[shm] ReplacePixelPlane {} -> {}x{} (target {}x{})",
            surface_id,
            new_max_w,
            new_max_h,
            target_w,
            target_h
        );
        Ok(())
    }
}

// Window options for creation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[napi(object)]
pub struct WindowOptions {
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
    pub fullscreen: bool,
    pub title: String,
    pub resizable: bool,
    pub frameless: bool,
    pub always_on_top: bool,
    pub initial_url: Option<String>,
    pub restore_state: bool,
    pub root: Option<String>,
    pub transparent: bool,
    pub corner_radius: Option<f64>,
    pub visible: bool,
    pub auto_resize_main: bool,
    pub panes: Vec<PaneOptions>,
    pub id: Option<String>,
    pub wm_class: Option<String>,
    pub scrollbars_enabled: Option<bool>,
    pub scrollbar_theme: Option<String>,
    pub scrollbar_auto_hide: Option<bool>,
    pub scrollbar_width: Option<u32>,
    pub trusted_origins: Option<Vec<String>>,
    /// Optional virtual layout tree from LayoutBuilder.config().layout (JSON string).
    pub layout: Option<String>,
}

#[napi(object)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneOptions {
    pub id: String,
    pub url: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub z_index: i32,
    pub visible: bool,
    pub anchor: Option<u32>,
    pub dock_order: Option<u32>,
}

impl Default for WindowOptions {
    fn default() -> Self {
        WindowOptions {
            width: 1024,
            height: 768,
            maximized: false,
            fullscreen: false,
            title: "Lotus App".to_string(),
            resizable: true,
            frameless: false,
            always_on_top: false,
            initial_url: None,
            restore_state: true,
            root: None,
            transparent: false,
            corner_radius: None,
            visible: true,
            auto_resize_main: true,
            panes: Vec::new(),
            id: None,
            wm_class: None,
            scrollbars_enabled: None,
            scrollbar_theme: None,
            scrollbar_auto_hide: None,
            scrollbar_width: None,
            trusted_origins: None,
            layout: None,
        }
    }
}

pub struct SelectPromptWrapper(pub servo::SelectElement);

impl std::fmt::Debug for SelectPromptWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SelectElement(id={:?})", self.0.id())
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct DragRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, serde::Deserialize)]
pub struct DragRegionPayload {
    #[serde(rename = "paneId")]
    pub pane_id: Option<String>,
    pub drag: Option<Vec<DragRect>>,
    #[serde(rename = "noDrag")]
    #[allow(non_snake_case)]
    pub no_drag: Option<Vec<DragRect>>,
}
