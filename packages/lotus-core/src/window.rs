use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, atomic::AtomicBool};

use dpi::PhysicalSize as ServoPhysicalSize;
use euclid::Point2D;
use napi_derive::napi;
use serde::{Deserialize, Serialize};
use servo::{Code, Key, Modifiers, WindowRenderingContext, OffscreenRenderingContext};
use winit::window::Window;

use crate::layout::{PaneAnchor, StagedLayout, StagedPaneLayout};

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
    
    vec4 tex_color = texture(u_texture, v_uv);
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
    pub offscreen_ctx: Rc<OffscreenRenderingContext>,
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
    pub is_remote: bool,
    pub remote_consumer: Option<crate::gl::RemotePaneConsumer>,
    pub remote_input_fd: Option<std::os::unix::io::RawFd>,
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
    pub comp_tex_size: Option<winit::dpi::PhysicalSize<u32>>,
    pub scene_fbo: Option<glow::NativeFramebuffer>,
    pub scene_tex: Option<glow::NativeTexture>,
    pub scene_tex_size: Option<winit::dpi::PhysicalSize<u32>>,
    pub committed_layout: Option<StagedLayout>,
    pub active_window_size: winit::dpi::PhysicalSize<u32>,
    pub committed_window_size: Option<winit::dpi::PhysicalSize<u32>>,
    pub active_select: Option<ActiveSelectPrompt>,
    pub scrollbar_config: ScrollbarConfig,
    pub trusted_origins: Vec<String>,
}

impl WindowInstance {
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
        let mut available_phys = euclid::Rect::new(
            euclid::default::Point2D::new(0, 0),
            euclid::default::Size2D::new(size.width as i32, size.height as i32)
        );

        let mut next_staged = StagedLayout {
            panes: HashMap::new(),
            width: size.width,
            height: size.height,
            scale_factor,
        };
        let mut sorted_ids: Vec<_> = self.panes.keys().cloned().collect();
        sorted_ids.sort_by_key(|id| self.panes.get(id).map(|p| p.dock_order).unwrap_or(0));

        // Pre-populate staged layout with current values to ensure all panes are tracked
        for id in &sorted_ids {
            let pane = self.panes.get(id).unwrap();
            next_staged.panes.insert(id.clone(), StagedPaneLayout {
                rect: pane.last_physical_rect,
                visible: pane.pending_visible,
            });
        }

        for id in sorted_ids {
            let pane = self.panes.get_mut(&id).unwrap();

            if !pane.pending_visible {
                continue;
            }

            let mut pane_phys = euclid::Rect::new(
                available_phys.origin,
                euclid::default::Size2D::new(0, 0)
            );

            match pane.anchor {
                PaneAnchor::Left => {
                    let pw = (pane.rect.size.width * scale_factor).round() as i32;
                    let clamped_w = pw.min(available_phys.size.width).max(0);
                    pane_phys.size = euclid::default::Size2D::new(clamped_w, available_phys.size.height);
                    available_phys.origin.x += clamped_w;
                    available_phys.size.width -= clamped_w;
                },
                PaneAnchor::Right => {
                    let pw = (pane.rect.size.width * scale_factor).round() as i32;
                    let clamped_w = pw.min(available_phys.size.width).max(0);
                    pane_phys.origin.x = available_phys.origin.x + available_phys.size.width - clamped_w;
                    pane_phys.size = euclid::default::Size2D::new(clamped_w, available_phys.size.height);
                    available_phys.size.width -= clamped_w;
                },
                PaneAnchor::Top => {
                    let ph = (pane.rect.size.height * scale_factor).round() as i32;
                    let clamped_h = ph.min(available_phys.size.height).max(0);
                    pane_phys.size = euclid::default::Size2D::new(available_phys.size.width, clamped_h);
                    pane_phys.origin.y += clamped_h;
                    available_phys.size.height -= clamped_h;
                },
                PaneAnchor::Bottom => {
                    let ph = (pane.rect.size.height * scale_factor).round() as i32;
                    let clamped_h = ph.min(available_phys.size.height).max(0);
                    pane_phys.origin.y = available_phys.origin.y + available_phys.size.height - clamped_h;
                    pane_phys.size = euclid::default::Size2D::new(available_phys.size.width, clamped_h);
                    available_phys.size.height -= clamped_h;
                },
                PaneAnchor::Fill => {
                    pane_phys = available_phys;
                    available_phys.size = euclid::default::Size2D::new(0, 0); // Consumed
                },
                PaneAnchor::None => {
                    // For non-anchored, use their specified rect snapped to physical
                    let phys_x = (pane.rect.origin.x * scale_factor).round() as i32;
                    let phys_y = (pane.rect.origin.y * scale_factor).round() as i32;
                    let phys_w = (pane.rect.size.width * scale_factor).round() as i32;
                    let phys_h = (pane.rect.size.height * scale_factor).round() as i32;
                    pane_phys = euclid::Rect::new(
                        euclid::default::Point2D::new(phys_x, phys_y),
                        euclid::default::Size2D::new(phys_w, phys_h)
                    );
                }
            }

            // Record target in staged layout
            if let Some(staged) = next_staged.panes.get_mut(&id) {
                staged.rect = pane_phys;
            }

            let servo_size = ServoPhysicalSize::new(
                (pane_phys.size.width as u32).max(1),
                (pane_phys.size.height as u32).max(1)
            );
            let logical_rect = euclid::Rect::new(
                euclid::Point2D::new(pane_phys.origin.x as f32 / scale_factor, pane_phys.origin.y as f32 / scale_factor),
                euclid::Size2D::new(pane_phys.size.width as f32 / scale_factor, pane_phys.size.height as f32 / scale_factor)
            );

            if pane.last_notified_rect != Some(logical_rect) || pane.is_dirty {
                pane.pending_physical_rect = Some(pane_phys);
                let size_changed = servo_size != pane.current_servo_size || !pane.first_frame_painted;

                // EVERY layout change (even position only) joins the transactional "Nuclear Flip"
                pane.is_resizing = true;

                if size_changed {
                    pane.pending_servo_size = Some(servo_size);
                }

                pane.last_notified_rect = Some(logical_rect);
                pane.is_dirty = false;
            }
        }
        self.staged_layout = next_staged;

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
            let needs_transaction = self.panes.values().any(|p| {
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
        // Forces Servo to render exactly what the locked transaction is waiting for.
        for pane in self.panes.values_mut() {
            if !pane.servo_busy {
                if let Some(committed) = &self.committed_layout {
                    if let Some(staged) = committed.panes.get(&pane.id) {
                        let target_size = ServoPhysicalSize::new(
                            (staged.rect.size.width as u32).max(1),
                            (staged.rect.size.height as u32).max(1)
                        );
                        if pane.current_servo_size != target_size || !pane.first_frame_painted {
                            pane.webview.resize(target_size);
                            pane.requested_servo_size = target_size;
                            pane.servo_busy = true;
                            pane.pending_servo_size = None; // Wipe any spurious queued sizes
                            pane.is_resizing = true;
                        }
                    }
                }
            }
        }

        if self.committed_layout.is_some() {
            self.window.request_redraw();
        }
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
