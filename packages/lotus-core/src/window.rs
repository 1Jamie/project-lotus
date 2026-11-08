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
    pub remote_consumer: Option<crate::gl::RemotePaneConsumer>,
    pub remote_input_fd: Option<std::os::unix::io::RawFd>,
    pub resize_generation: u64,
    /// Host crash chrome is covering this remote — still pull SHM/vsync, but do not
    /// draw the (possibly stale) texture into the scene.
    pub under_crash_cover: bool,
    /// After a respawn, emit `remote-frame-composited` once a fresh SHM frame is
    /// uploaded so JS can lift crash chrome without flashing the prior life.
    pub await_host_composited_frame: bool,
}

impl Drop for PaneInstance {
    fn drop(&mut self) {
        if let Some(fd) = self.remote_input_fd.take() {
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
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
    pub comp_fbo: Option<glow::NativeFramebuffer>,
    pub comp_tex: Option<glow::NativeTexture>,
    pub comp_vao: Option<glow::VertexArray>,
    pub comp_vbo: Option<glow::NativeBuffer>,
    pub u_comp_size_loc: Option<glow::UniformLocation>,
    pub u_comp_radius_loc: Option<glow::UniformLocation>,
    pub u_comp_flip_loc: Option<glow::UniformLocation>,
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
    /// Last pane that received a routed MouseMove (for synthetic leave on gutter entry).
    pub hover_pane_id: Option<String>,
}

impl WindowInstance {
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
            self.gl.delete_shader(vs);
            self.gl.detach_shader(program, fs);
            self.gl.delete_shader(fs);

            self.u_comp_size_loc = self.gl.get_uniform_location(program, "u_size");
            self.u_comp_radius_loc = self.gl.get_uniform_location(program, "u_radius");
            self.u_comp_flip_loc = self.gl.get_uniform_location(program, "u_flip_y");
            self.comp_program = Some(program);

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

            let pane_phys = if let Some(rect) = solved.get(&id) {
                if !pane.pending_visible {
                    continue;
                }
                *rect
            } else if pane.anchor == PaneAnchor::None {
                if !pane.pending_visible {
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
                let size_changed = servo_size != pane.current_servo_size || !pane.first_frame_painted;
                pane.is_resizing = true;
                if size_changed {
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
                !self.panes.values().any(|p| p.servo_busy)
            }
        };

        if can_commit {
            let needs_transaction = self.active_window_size != size || self.panes.values().any(|p| {
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
