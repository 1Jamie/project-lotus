use std::collections::HashSet;
use glow::HasContext;
use servo::RenderingContext;
use crate::window::WindowInstance;

pub fn ensure_textures(instance: &mut WindowInstance, active_w: u32, active_h: u32) {
    let current_safe_size = winit::dpi::PhysicalSize::new(active_w, active_h);
    unsafe {
        if instance.scene_tex_size != Some(current_safe_size) {
            if instance.scene_fbo.is_none() {
                instance.scene_fbo = Some(instance.gl.create_framebuffer().unwrap());
                instance.scene_tex = Some(instance.gl.create_texture().unwrap());
            }
            if let Some(tex) = instance.scene_tex {
                instance.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                instance.gl.tex_image_2d(
                    glow::TEXTURE_2D, 0, glow::RGBA8 as i32,
                    active_w as i32, active_h as i32,
                    0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None),
                );
                instance.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                instance.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                instance.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                instance.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

                instance.gl.bind_framebuffer(glow::FRAMEBUFFER, instance.scene_fbo);
                instance.gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
            }
            instance.scene_tex_size = Some(current_safe_size);
        }

        if instance.comp_tex_size != Some(current_safe_size) {
            if let Some(tex) = instance.comp_tex {
                instance.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                instance.gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, active_w as i32, active_h as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
                instance.gl.bind_texture(glow::TEXTURE_2D, None);
                instance.comp_tex_size = Some(current_safe_size);
            }
        }
    }
}

/// Remote pull results for one composite pass.
pub struct CompositeRemoteResult {
    /// Remotes that consumed a new SHM frame (host vsync pull loop).
    pub remotes_with_new_frames: Vec<String>,
    /// Remotes whose first post-respawn frame is now in the GL texture — safe to
    /// lift crash chrome without flashing the prior life's pixels.
    pub cover_ready_panes: Vec<String>,
}

/// True when the host must rebuild the scene FBO and issue a swapchain present.
/// Idle parking: skip clear/bind/present when pane pixels and layout are unchanged
/// so we do not re-arm Wayland frame callbacks at display rate with no damage.
pub fn scene_needs_present(
    instance: &WindowInstance,
    window_size: winit::dpi::PhysicalSize<u32>,
    flip_occurred: bool,
    pending_hides: &HashSet<String>,
) -> bool {
    #[cfg(target_os = "linux")]
    if instance.overlay_dirty
        && crate::pacing::present_activity(instance) == crate::pacing::PresentActivity::UiIdle
    {
        return true;
    }
    if flip_occurred || instance.committed_layout.is_some() || instance.force_scene_repaint {
        return true;
    }
    for (id, staged) in &instance.staged_layout.panes {
        if let Some(active) = instance.active_layout.panes.get(id) {
            if staged.visible != active.visible {
                return true;
            }
        }
    }
    if instance.last_presented_window_size != Some(window_size) {
        return true;
    }
    if !pending_hides.is_empty() {
        return true;
    }

    // Fixed SoftFifo hold: re-present the last scene on spare vblanks even when
    // no pane seq advanced (cadence wake with unchanged buffers).
    #[cfg(target_os = "linux")]
    if crate::pacing::wants_fixed_hold_cadence(instance) {
        return true;
    }

    // VRR GameActive: only primary nest damage may drive a swap. Overlay-only or
    // X11 expose wakes must not free-run at panel max (breaks passthrough).
    #[cfg(target_os = "linux")]
    if crate::pacing::vrr_requires_primary_damage(instance) {
        return crate::pacing::primary_has_unpresented_frame(instance);
    }

    for pane in instance.panes.values() {
        if !pane.is_visible && !pane.pending_visible {
            continue;
        }
        if pane.is_resizing {
            return true;
        }
        // Visible (or becoming visible) panes with newer content must present.
        if pane.is_visible || pane.pending_visible {
            if pane.frame_seq != pane.last_presented_seq {
                return true;
            }
        }
    }
    false
}

/// After a successful present, latch pane + window damage state.
pub fn ack_presented_scene(
    instance: &mut WindowInstance,
    window_size: winit::dpi::PhysicalSize<u32>,
) {
    instance.force_scene_repaint = false;
    #[cfg(target_os = "linux")]
    {
        instance.overlay_dirty = false;
    }
    instance.last_presented_window_size = Some(window_size);
    for pane in instance.panes.values_mut() {
        if pane.is_visible {
            pane.last_presented_seq = pane.frame_seq;
        }
    }
}

fn draw_pane_texture(
    instance: &WindowInstance,
    tex: glow::NativeTexture,
    layout_px: i32,
    layout_py: i32,
    layout_pw: i32,
    layout_ph: i32,
    scale_factor: f32,
    flip_y: i32,
) {
    // Local/Servo panes: bilinear only (no nest upscale / RCAS temp).
    draw_upscale_pass(
        instance,
        tex,
        glow::TEXTURE_2D,
        layout_px,
        layout_py,
        layout_pw,
        layout_ph,
        layout_px,
        layout_py,
        layout_pw,
        layout_ph,
        scale_factor,
        flip_y,
        false,
        None,
        crate::window::HostUpscale::Off,
        false,
        instance.corner_radius.unwrap_or(0.0) as f32 * scale_factor,
    );
}

/// GL_TEXTURE_EXTERNAL_OES — must bind with this target, not TEXTURE_2D.
const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

#[cfg(target_os = "linux")]
fn ensure_upscale_temp(instance: &mut WindowInstance, w: u32, h: u32) {
    let w = w.max(1);
    let h = h.max(1);
    if instance.upscale_temp_size == Some((w, h)) {
        return;
    }
    unsafe {
        if instance.upscale_temp_tex.is_none() {
            instance.upscale_temp_tex = Some(instance.gl.create_texture().unwrap());
        }
        if instance.upscale_temp_fbo.is_none() {
            instance.upscale_temp_fbo = Some(instance.gl.create_framebuffer().unwrap());
        }
        let tex = instance.upscale_temp_tex.unwrap();
        instance.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        instance.gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            w as i32,
            h as i32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        instance.gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::NEAREST as i32,
        );
        instance.gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::NEAREST as i32,
        );
        instance.gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        instance.gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        let fbo = instance.upscale_temp_fbo.unwrap();
        instance.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        instance.gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(tex),
            0,
        );
        instance.gl.bind_framebuffer(glow::FRAMEBUFFER, instance.scene_fbo);
        instance.gl.bind_texture(glow::TEXTURE_2D, None);
        instance.upscale_temp_size = Some((w, h));
    }
}

fn draw_pane_texture_ext(
    instance: &mut WindowInstance,
    tex: glow::NativeTexture,
    target: u32,
    layout_px: i32,
    layout_py: i32,
    layout_pw: i32,
    layout_ph: i32,
    scale_factor: f32,
    flip_y: i32,
    opaque: bool,
    src_buf: Option<(u32, u32)>,
    upscale: crate::window::HostUpscale,
) {
    draw_pane_texture_ext_clipped(
        instance,
        tex,
        target,
        layout_px,
        layout_py,
        layout_pw,
        layout_ph,
        layout_px,
        layout_py,
        layout_pw,
        layout_ph,
        scale_factor,
        flip_y,
        opaque,
        src_buf,
        upscale,
    );
}

/// Like [`draw_pane_texture_ext`], but viewport (draw dest) may differ from scissor
/// (clip). Used for cover-fit overlays: draw a larger quad, clip to the dock pane.
/// `opaque`: primary/game fullscreen blit — skip blending for a pure 1:1 copy.
///
/// When `ATRIUM_SHARPEN=rcas`: Pass1 upscales into an upright NEAREST temp (Y-flip
/// applied here); Pass2 RCAS → scene with no flip (`FragCoord - origin`).
fn draw_pane_texture_ext_clipped(
    instance: &mut WindowInstance,
    tex: glow::NativeTexture,
    target: u32,
    viewport_px: i32,
    viewport_py: i32,
    viewport_pw: i32,
    viewport_ph: i32,
    clip_px: i32,
    clip_py: i32,
    clip_pw: i32,
    clip_ph: i32,
    scale_factor: f32,
    flip_y: i32,
    opaque: bool,
    src_buf: Option<(u32, u32)>,
    upscale: crate::window::HostUpscale,
) {
    #[cfg(target_os = "linux")]
    let want_rcas =
        crate::window::host_sharpen_rcas() && instance.fsr1_rcas_program.is_some();
    #[cfg(not(target_os = "linux"))]
    let want_rcas = false;

    #[cfg(target_os = "linux")]
    if want_rcas {
        ensure_upscale_temp(instance, viewport_pw.max(1) as u32, viewport_ph.max(1) as u32);
        // Pass 1: source → upright temp (apply Y-flip here).
        draw_upscale_pass(
            instance,
            tex,
            target,
            0,
            0,
            viewport_pw,
            viewport_ph,
            0,
            0,
            viewport_pw,
            viewport_ph,
            scale_factor,
            flip_y,
            opaque,
            src_buf,
            upscale,
            true, // to_temp
            0.0,  // no corner mask in temp
        );
        // Pass 2: temp → scene, no flip, RCAS.
        if let (Some(rcas), Some(temp_tex)) =
            (instance.fsr1_rcas_program, instance.upscale_temp_tex)
        {
            unsafe {
                instance.gl.bind_framebuffer(glow::FRAMEBUFFER, instance.scene_fbo);
                instance
                    .gl
                    .viewport(viewport_px, viewport_py, viewport_pw, viewport_ph);
                instance.gl.enable(glow::SCISSOR_TEST);
                instance.gl.scissor(clip_px, clip_py, clip_pw, clip_ph);
                if opaque {
                    instance.gl.disable(glow::BLEND);
                } else {
                    instance.gl.enable(glow::BLEND);
                    instance.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                }
                instance.gl.use_program(Some(rcas));
                instance.gl.bind_vertex_array(instance.comp_vao);
                instance.gl.active_texture(glow::TEXTURE0);
                instance.gl.bind_texture(glow::TEXTURE_2D, Some(temp_tex));
                let radius = instance.corner_radius.unwrap_or(0.0) as f32 * scale_factor;
                let locs = &instance.fsr1_locs;
                instance
                    .gl
                    .uniform_2_f32(locs.rcas_size.as_ref(), viewport_pw as f32, viewport_ph as f32);
                instance.gl.uniform_1_f32(locs.rcas_radius.as_ref(), radius);
                instance.gl.uniform_2_f32(
                    locs.rcas_origin.as_ref(),
                    viewport_px as f32,
                    viewport_py as f32,
                );
                let con = crate::gl::fsr1::rcas_con(crate::window::rcas_sharpness_stops());
                instance
                    .gl
                    .uniform_4_f32(locs.rcas_con.as_ref(), con[0], con[1], con[2], con[3]);
                instance.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                instance.gl.bind_texture(glow::TEXTURE_2D, None);
                instance.gl.bind_vertex_array(None);
                instance.gl.use_program(None);
                instance.gl.disable(glow::SCISSOR_TEST);
            }
            return;
        }
    }

    draw_upscale_pass(
        instance,
        tex,
        target,
        viewport_px,
        viewport_py,
        viewport_pw,
        viewport_ph,
        clip_px,
        clip_py,
        clip_pw,
        clip_ph,
        scale_factor,
        flip_y,
        opaque,
        src_buf,
        upscale,
        false,
        instance.corner_radius.unwrap_or(0.0) as f32 * scale_factor,
    );
}

fn draw_upscale_pass(
    instance: &WindowInstance,
    tex: glow::NativeTexture,
    target: u32,
    viewport_px: i32,
    viewport_py: i32,
    viewport_pw: i32,
    viewport_ph: i32,
    clip_px: i32,
    clip_py: i32,
    clip_pw: i32,
    clip_ph: i32,
    _scale_factor: f32,
    flip_y: i32,
    opaque: bool,
    src_buf: Option<(u32, u32)>,
    upscale: crate::window::HostUpscale,
    to_temp: bool,
    radius: f32,
) {
    unsafe {
        #[cfg(target_os = "linux")]
        if to_temp {
            if let Some(fbo) = instance.upscale_temp_fbo {
                instance.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            }
        } else {
            instance.gl.bind_framebuffer(glow::FRAMEBUFFER, instance.scene_fbo);
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = to_temp;
            instance.gl.bind_framebuffer(glow::FRAMEBUFFER, instance.scene_fbo);
        }

        instance
            .gl
            .viewport(viewport_px, viewport_py, viewport_pw, viewport_ph);
        instance.gl.enable(glow::SCISSOR_TEST);
        instance.gl.scissor(clip_px, clip_py, clip_pw, clip_ph);
        if opaque {
            instance.gl.disable(glow::BLEND);
        } else {
            instance.gl.enable(glow::BLEND);
            instance.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
        }

        let use_oes = target == GL_TEXTURE_EXTERNAL_OES;
        let bilinear = if use_oes {
            (
                instance.comp_program_oes.or(instance.comp_program),
                instance
                    .u_comp_oes_size_loc
                    .as_ref()
                    .or(instance.u_comp_size_loc.as_ref()),
                instance
                    .u_comp_oes_radius_loc
                    .as_ref()
                    .or(instance.u_comp_radius_loc.as_ref()),
                instance
                    .u_comp_oes_flip_loc
                    .as_ref()
                    .or(instance.u_comp_flip_loc.as_ref()),
            )
        } else {
            (
                instance.comp_program,
                instance.u_comp_size_loc.as_ref(),
                instance.u_comp_radius_loc.as_ref(),
                instance.u_comp_flip_loc.as_ref(),
            )
        };

        let do_upscale = src_buf.is_some() && upscale != crate::window::HostUpscale::Off;

        #[cfg(target_os = "linux")]
        if do_upscale && upscale == crate::window::HostUpscale::Fsr1 {
            let (program, oes) = if use_oes {
                (instance.fsr1_easu_program_oes, true)
            } else {
                (instance.fsr1_easu_program, false)
            };
            if let (Some(program), Some((sw, sh))) = (program, src_buf) {
                let locs = &instance.fsr1_locs;
                let (size_l, radius_l, flip_l, origin_l, src_l, texel_l, c0, c1, c2, c3) = if oes {
                    (
                        locs.easu_oes_size.as_ref(),
                        locs.easu_oes_radius.as_ref(),
                        locs.easu_oes_flip.as_ref(),
                        locs.easu_oes_origin.as_ref(),
                        locs.easu_oes_src.as_ref(),
                        locs.easu_oes_texel.as_ref(),
                        locs.easu_oes_con0.as_ref(),
                        locs.easu_oes_con1.as_ref(),
                        locs.easu_oes_con2.as_ref(),
                        locs.easu_oes_con3.as_ref(),
                    )
                } else {
                    (
                        locs.easu_size.as_ref(),
                        locs.easu_radius.as_ref(),
                        locs.easu_flip.as_ref(),
                        locs.easu_origin.as_ref(),
                        locs.easu_src.as_ref(),
                        locs.easu_texel.as_ref(),
                        locs.easu_con0.as_ref(),
                        locs.easu_con1.as_ref(),
                        locs.easu_con2.as_ref(),
                        locs.easu_con3.as_ref(),
                    )
                };
                let (con0, con1, con2, con3) = crate::gl::fsr1::easu_con(
                    sw as f32,
                    sh as f32,
                    viewport_pw.max(1) as f32,
                    viewport_ph.max(1) as f32,
                );
                instance.gl.use_program(Some(program));
                instance.gl.bind_vertex_array(instance.comp_vao);
                instance.gl.active_texture(glow::TEXTURE0);
                instance.gl.bind_texture(target, Some(tex));
                instance
                    .gl
                    .uniform_2_f32(size_l, viewport_pw as f32, viewport_ph as f32);
                instance.gl.uniform_1_f32(radius_l, radius);
                instance.gl.uniform_1_i32(flip_l, flip_y);
                instance
                    .gl
                    .uniform_2_f32(origin_l, viewport_px as f32, viewport_py as f32);
                let sw = sw.max(1) as f32;
                let sh = sh.max(1) as f32;
                instance.gl.uniform_2_f32(src_l, sw, sh);
                instance.gl.uniform_2_f32(texel_l, 1.0 / sw, 1.0 / sh);
                instance
                    .gl
                    .uniform_4_f32(c0, con0[0], con0[1], con0[2], con0[3]);
                instance
                    .gl
                    .uniform_4_f32(c1, con1[0], con1[1], con1[2], con1[3]);
                instance
                    .gl
                    .uniform_4_f32(c2, con2[0], con2[1], con2[2], con2[3]);
                instance
                    .gl
                    .uniform_4_f32(c3, con3[0], con3[1], con3[2], con3[3]);
                instance.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                instance.gl.bind_texture(target, None);
                instance.gl.bind_vertex_array(None);
                instance.gl.use_program(None);
                instance.gl.disable(glow::SCISSOR_TEST);
                return;
            }
        }

        #[cfg(target_os = "linux")]
        let (program, size_loc, radius_loc, flip_loc, texel_loc, src_loc) =
            if do_upscale && upscale == crate::window::HostUpscale::Sgsr && use_oes {
                if let Some(p) = instance.sgsr_program_oes {
                    (
                        Some(p),
                        instance.u_sgsr_oes_size_loc.as_ref(),
                        instance.u_sgsr_oes_radius_loc.as_ref(),
                        instance.u_sgsr_oes_flip_loc.as_ref(),
                        instance.u_sgsr_oes_texel_loc.as_ref(),
                        instance.u_sgsr_oes_src_loc.as_ref(),
                    )
                } else {
                    (bilinear.0, bilinear.1, bilinear.2, bilinear.3, None, None)
                }
            } else if do_upscale && upscale == crate::window::HostUpscale::Sgsr {
                if let Some(p) = instance.sgsr_program {
                    (
                        Some(p),
                        instance.u_sgsr_size_loc.as_ref(),
                        instance.u_sgsr_radius_loc.as_ref(),
                        instance.u_sgsr_flip_loc.as_ref(),
                        instance.u_sgsr_texel_loc.as_ref(),
                        instance.u_sgsr_src_loc.as_ref(),
                    )
                } else {
                    (bilinear.0, bilinear.1, bilinear.2, bilinear.3, None, None)
                }
            } else {
                (bilinear.0, bilinear.1, bilinear.2, bilinear.3, None, None)
            };
        #[cfg(not(target_os = "linux"))]
        let (program, size_loc, radius_loc, flip_loc, texel_loc, src_loc) = {
            let _ = (src_buf, upscale, do_upscale);
            (bilinear.0, bilinear.1, bilinear.2, bilinear.3, None, None)
        };

        if let Some(program) = program {
            instance.gl.use_program(Some(program));
            instance.gl.bind_vertex_array(instance.comp_vao);
            instance.gl.active_texture(glow::TEXTURE0);
            instance.gl.bind_texture(target, Some(tex));

            instance
                .gl
                .uniform_2_f32(size_loc, viewport_pw as f32, viewport_ph as f32);
            instance.gl.uniform_1_f32(radius_loc, radius);
            instance.gl.uniform_1_i32(flip_loc, flip_y);
            #[cfg(target_os = "linux")]
            if let Some((sw, sh)) = src_buf {
                if do_upscale && upscale == crate::window::HostUpscale::Sgsr {
                    let sw = sw.max(1) as f32;
                    let sh = sh.max(1) as f32;
                    instance.gl.uniform_2_f32(texel_loc, 1.0 / sw, 1.0 / sh);
                    instance.gl.uniform_2_f32(src_loc, sw, sh);
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (texel_loc, src_loc);
            }

            instance.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            instance.gl.bind_texture(target, None);
            instance.gl.bind_vertex_array(None);
            instance.gl.use_program(None);
        }
        instance.gl.disable(glow::SCISSOR_TEST);
    }
}

/// Capture pane-sized last-good pixels into `ghost_tex` for stretch-interim during resize.
/// Call **after** the last live draw into the offscreen FBO and **before** that FBO is
/// resized or cleared. Sets `ghost_valid` on success.
pub(crate) fn capture_local_ghost(instance: &mut WindowInstance, pane_id: &str) {
    let Some(pane) = instance.panes.get_mut(pane_id) else {
        return;
    };
    if pane.is_remote {
        return;
    }
    let size = pane.offscreen_ctx.size();
    if size.width == 0 || size.height == 0 {
        return;
    }
    unsafe {
        if pane.ghost_tex.is_none() {
            pane.ghost_tex = Some(instance.gl.create_texture().unwrap());
        }
        let ghost = pane.ghost_tex.unwrap();
        let need_alloc = pane.ghost_tex_size
            != Some(winit::dpi::PhysicalSize::new(size.width, size.height));
        instance.gl.bind_texture(glow::TEXTURE_2D, Some(ghost));
        if need_alloc {
            instance.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                size.width as i32,
                size.height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            instance.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            instance.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            instance.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            instance.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            pane.ghost_tex_size = Some(winit::dpi::PhysicalSize::new(size.width, size.height));
        }
        pane.offscreen_ctx.bind_color_for_read(&instance.gl);
        instance.gl.bind_texture(glow::TEXTURE_2D, Some(ghost));
        instance.gl.copy_tex_sub_image_2d(
            glow::TEXTURE_2D,
            0,
            0,
            0,
            0,
            0,
            size.width as i32,
            size.height as i32,
        );
        instance.gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
        instance.gl.bind_texture(glow::TEXTURE_2D, None);
        pane.ghost_valid = true;
    }
}

/// Delete the local ghost texture (call before removing a pane from the map).
pub(crate) fn destroy_local_ghost(instance: &mut WindowInstance, pane_id: &str) {
    let Some(pane) = instance.panes.get_mut(pane_id) else {
        return;
    };
    if let Some(tex) = pane.ghost_tex.take() {
        unsafe {
            instance.gl.delete_texture(tex);
        }
    }
    pane.ghost_tex_size = None;
    pane.ghost_valid = false;
}

/// Returns pane ids of remote surfaces that consumed a new SHM frame this pass
/// (used to emit host Vsync only to remotes that are actually in the pull loop).
pub fn composite_panes_to_scene(
    instance: &mut WindowInstance,
    active_w: u32,
    active_h: u32,
    scale_factor: f32,
    is_transaction_pending: bool,
    transaction_complete: bool,
    pending_hides: &HashSet<String>,
) -> CompositeRemoteResult {
    let remotes_with_new_frames = Vec::new();
    let cover_ready_panes = Vec::new();

    // Experimental hole-punch only — never run punch bookkeeping on the reliable path.
    #[cfg(target_os = "linux")]
    let hole_punch = crate::gl::webgl_broker::experimental_hole_punch_enabled();
    #[cfg(target_os = "linux")]
    let mut hole_punch_panes: Vec<(String, i32, i32, i32, i32, u64)> = Vec::new();

    unsafe {
        // Defensive: never let a prior remote upload leave UNPACK_* set for Servo/WebRender.
        instance.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        instance.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        instance.gl.bind_framebuffer(glow::FRAMEBUFFER, instance.scene_fbo);
        instance.gl.disable(glow::SCISSOR_TEST);
        instance.gl.viewport(0, 0, active_w as i32, active_h as i32);

        instance.gl.color_mask(true, true, true, true);
        // Game mode + primary covering the viewport: skip clear (opaque blit replaces all).
        // Nest external surfaces exist only on Linux; keep Windows compiling as a noop.
        #[cfg(target_os = "linux")]
        let skip_clear = instance.game_mode
            && instance.primary_game_pane.as_ref().is_some_and(|pid| {
                instance
                    .panes
                    .get(pid)
                    .and_then(|p| p.external_surface_slot.as_ref())
                    .is_some_and(|s| s.lock().current_texture.is_some())
            });
        #[cfg(not(target_os = "linux"))]
        let skip_clear = false;
        if !skip_clear {
            if instance.transparent {
                instance.gl.clear_color(0.0, 0.0, 0.0, 0.0);
            } else {
                // Match normal gamescope/Steam black splash (no diagnostic navy flash).
                instance.gl.clear_color(0.0, 0.0, 0.0, 1.0);
            }
            instance
                .gl
                .clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT | glow::STENCIL_BUFFER_BIT);
        }

        // Prefer pending rect for same-window reflows (weights/dock/gutter) so the
        // slot snaps while the old texture stretches until nuclear flip.
        // Skip when staged was solved for a different window size than the active
        // scene — otherwise downsize jumps ahead and leaves black bars until flip.
        let pending_fits_active = instance.staged_layout.width == active_w
            && instance.staged_layout.height == active_h;

        let mut sorted_ids: Vec<_> = instance.panes.keys().cloned().collect();
        sorted_ids.sort_by_key(|id| {
            instance.panes.get(id).map(|p| {
                #[cfg(target_os = "linux")]
                let is_overlay = if p.external_wayland_server.is_some() { 0 } else { 1 };
                #[cfg(not(target_os = "linux"))]
                let is_overlay = 0;
                (p.z_index, is_overlay, p.dock_order)
            }).unwrap_or((0, 0, 0))
        });

        for id in sorted_ids {
            enum DrawPlan {
                Skip,
                Remote { tex: glow::NativeTexture },
                LocalFreeze { tex: glow::NativeTexture },
                LocalLive { tex: glow::NativeTexture, capture: bool },
                #[cfg(target_os = "linux")]
                ExternalSurface {
                    tex: glow::NativeTexture,
                    target: u32,
                    flip_y: i32,
                    buf_w: u32,
                    buf_h: u32,
                    opaque: bool,
                    upscale: crate::window::HostUpscale,
                },
            }

            let (layout_px, layout_py, layout_pw, layout_ph, plan) = {
                let Some(pane) = instance.panes.get(&id) else {
                    continue;
                };
                let composite_visible = instance
                    .staged_layout
                    .panes
                    .get(&id)
                    .map(|s| s.visible)
                    .unwrap_or(pane.is_visible);
                if !composite_visible {
                    continue;
                }
                // Same-window interim: always use staged_layout for EVERY pane so
                // neighbors cannot diverge (pending armed on one, last_physical on
                // another → black seam). Textures stretch into these slots.
                // Window-size interim (!pending_fits_active): prefer active_layout so
                // an eager last_physical writer cannot place into the old scene FBO.
                let rect = if pending_fits_active {
                    instance
                        .staged_layout
                        .panes
                        .get(&id)
                        .map(|s| s.rect)
                        .filter(|r| r.size.width > 0 && r.size.height > 0)
                        .or_else(|| {
                            pane.pending_physical_rect
                                .filter(|r| r.size.width > 0 && r.size.height > 0)
                        })
                        .unwrap_or(pane.last_physical_rect)
                } else {
                    instance
                        .active_layout
                        .panes
                        .get(&id)
                        .map(|s| s.rect)
                        .filter(|r| r.size.width > 0 && r.size.height > 0)
                        .unwrap_or(pane.last_physical_rect)
                };
                if rect.size.width <= 0 || rect.size.height <= 0 {
                    continue;
                }
                let layout_pw = rect.size.width;
                let layout_ph = rect.size.height;
                let layout_px = rect.origin.x;
                let layout_py = active_h as i32 - (rect.origin.y + layout_ph);

                #[cfg(target_os = "linux")]
                let plan = if let Some(ref slot) = pane.external_surface_slot {
                    let guard = slot.lock();
                    if let Some(tex) = guard.current_texture {
                        // Game and overlay panes scan out Wayland DMA-BUFs/SHM directly.
                        // Standard Wayland buffers are top-down (row 0 at top), so we flip V
                        // unless the buffer was explicitly flagged as bottom-up (y_inverted).
                        let is_primary = instance
                            .primary_game_pane
                            .as_ref()
                            .is_some_and(|pid| pid == &pane.id);
                        DrawPlan::ExternalSurface {
                            tex: glow::NativeTexture(tex.0),
                            target: guard.target,
                            flip_y: if guard.y_inverted { 0 } else { 1 },
                            buf_w: guard.width.max(1),
                            buf_h: guard.height.max(1),
                            // Opaque primary blit in game mode — no blend tax.
                            opaque: instance.game_mode && is_primary,
                            upscale: if pane.external_client_upscale_sgsr {
                                crate::window::HostUpscale::Sgsr
                            } else if is_primary {
                                crate::window::host_upscale()
                            } else {
                                crate::window::HostUpscale::Off
                            },
                        }
                    } else {
                        DrawPlan::Skip
                    }
                } else if pane.is_remote {
                    if pane.under_crash_cover {
                        DrawPlan::Skip
                    } else if let Some(tex) = pane
                        .remote_consumer
                        .as_ref()
                        .and_then(|c| c.get_texture())
                    {
                        DrawPlan::Remote { tex }
                    } else {
                        DrawPlan::Skip
                    }
                } else {
                    let is_locked = is_transaction_pending && !transaction_complete;
                    let is_hiding = pending_hides.contains(&pane.id);
                    // Hide→show must sample the live FBO, not a freeze ghost — otherwise
                    // HUD/telemetry panes stick on the last pre-hide frame until reflow.
                    let becoming_visible = pane.pending_visible && !pane.is_visible;
                    let want_freeze =
                        (is_locked || pane.is_resizing || is_hiding) && !becoming_visible;
                    let tex_size = pane.offscreen_ctx.size();
                    let live_ok = tex_size.width > 0 && tex_size.height > 0;
                    if want_freeze && pane.ghost_valid {
                        if let Some(tex) = pane.ghost_tex {
                            DrawPlan::LocalFreeze { tex }
                        } else if live_ok {
                            // ghost_valid without tex should not happen; fill live + seed.
                            DrawPlan::LocalLive {
                                tex: pane.offscreen_ctx.color_texture(),
                                capture: true,
                            }
                        } else {
                            DrawPlan::Skip
                        }
                    } else if want_freeze && live_ok {
                        // Freeze requested but no valid ghost yet (e.g. hide/transaction
                        // without a prior resize seed) — draw live and capture once.
                        DrawPlan::LocalLive {
                            tex: pane.offscreen_ctx.color_texture(),
                            capture: true,
                        }
                    } else if live_ok {
                        // Steady-state: no per-frame ghost copy.
                        DrawPlan::LocalLive {
                            tex: pane.offscreen_ctx.color_texture(),
                            capture: false,
                        }
                    } else {
                        DrawPlan::Skip
                    }
                };

                #[cfg(not(target_os = "linux"))]
                let plan = if pane.is_remote {
                    if pane.under_crash_cover {
                        DrawPlan::Skip
                    } else if let Some(tex) = pane
                        .remote_consumer
                        .as_ref()
                        .and_then(|c| c.get_texture())
                    {
                        DrawPlan::Remote { tex }
                    } else {
                        DrawPlan::Skip
                    }
                } else {
                    let is_locked = is_transaction_pending && !transaction_complete;
                    let is_hiding = pending_hides.contains(&pane.id);
                    let becoming_visible = pane.pending_visible && !pane.is_visible;
                    let want_freeze =
                        (is_locked || pane.is_resizing || is_hiding) && !becoming_visible;
                    let tex_size = pane.offscreen_ctx.size();
                    let live_ok = tex_size.width > 0 && tex_size.height > 0;
                    if want_freeze && pane.ghost_valid {
                        if let Some(tex) = pane.ghost_tex {
                            DrawPlan::LocalFreeze { tex }
                        } else if live_ok {
                            DrawPlan::LocalLive {
                                tex: pane.offscreen_ctx.color_texture(),
                                capture: true,
                            }
                        } else {
                            DrawPlan::Skip
                        }
                    } else if want_freeze && live_ok {
                        DrawPlan::LocalLive {
                            tex: pane.offscreen_ctx.color_texture(),
                            capture: true,
                        }
                    } else if live_ok {
                        DrawPlan::LocalLive {
                            tex: pane.offscreen_ctx.color_texture(),
                            capture: false,
                        }
                    } else {
                        DrawPlan::Skip
                    }
                };
                (layout_px, layout_py, layout_pw, layout_ph, plan)
            };

            match plan {
                DrawPlan::Skip => {}
                DrawPlan::Remote { tex } => {
                    draw_pane_texture(
                        instance,
                        tex,
                        layout_px,
                        layout_py,
                        layout_pw,
                        layout_ph,
                        scale_factor,
                        1,
                    );
                    #[cfg(target_os = "linux")]
                    if hole_punch {
                        let shm_seq = instance
                            .panes
                            .get(&id)
                            .and_then(|p| p.remote_consumer.as_ref())
                            .map(|c| c.last_rendered_seq)
                            .unwrap_or(0);
                        crate::gl::webgl_broker::note_pane_shm_seq(&id, shm_seq);
                        hole_punch_panes.push((
                            id.clone(),
                            layout_px,
                            layout_py,
                            layout_pw,
                            layout_ph,
                            shm_seq,
                        ));
                    }
                }
                DrawPlan::LocalFreeze { tex } => {
                    draw_pane_texture(
                        instance,
                        tex,
                        layout_px,
                        layout_py,
                        layout_pw,
                        layout_ph,
                        scale_factor,
                        0,
                    );
                }
                DrawPlan::LocalLive { tex, capture } => {
                    draw_pane_texture(
                        instance,
                        tex,
                        layout_px,
                        layout_py,
                        layout_pw,
                        layout_ph,
                        scale_factor,
                        0,
                    );
                    if capture {
                        capture_local_ghost(instance, &id);
                    }
                }
                #[cfg(target_os = "linux")]
                DrawPlan::ExternalSurface {
                    tex,
                    target,
                    flip_y,
                    buf_w,
                    buf_h,
                    opaque,
                    upscale,
                } => {
                    // Contain-fit: scale the client buffer down to fully fit the pane
                    // (game letterbox + Discord drawer). Never crop / spill past bounds.
                    let pw = layout_pw.max(1) as f32;
                    let ph = layout_ph.max(1) as f32;
                    let bw = buf_w as f32;
                    let bh = buf_h as f32;
                    let pane_ar = pw / ph;
                    let buf_ar = bw / bh;
                    let (dx, dy, dw, dh) = if (pane_ar - buf_ar).abs() < 0.02 {
                        (layout_px, layout_py, layout_pw, layout_ph)
                    } else if buf_ar > pane_ar {
                        let dh = (pw / buf_ar).round().max(1.0) as i32;
                        let dy = layout_py + (layout_ph - dh) / 2;
                        (layout_px, dy, layout_pw, dh)
                    } else {
                        let dw = (ph * buf_ar).round().max(1.0) as i32;
                        let dx = layout_px + (layout_pw - dw) / 2;
                        (dx, layout_py, dw, layout_ph)
                    };
                    let src_buf = if upscale != crate::window::HostUpscale::Off
                        && (dw > buf_w as i32 + 1 || dh > buf_h as i32 + 1)
                    {
                        Some((buf_w, buf_h))
                    } else {
                        None
                    };
                    let upscale = if src_buf.is_some() {
                        upscale
                    } else {
                        crate::window::HostUpscale::Off
                    };
                    draw_pane_texture_ext(
                        instance,
                        tex,
                        target,
                        dx,
                        dy,
                        dw,
                        dh,
                        scale_factor,
                        flip_y,
                        opaque,
                        src_buf,
                        upscale,
                    );
                }
            }
        }

        instance.gl.color_mask(true, true, true, true);
        instance.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        instance.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        // The pane draw above is the sample of the client's wl_buffer. Record a
        // fence after it, then flush, so a later present can prove that sample
        // finished before the buffer is handed back.
        #[cfg(target_os = "linux")]
        if instance.pacing_policy == crate::pacing::PacingPolicy::FixedVsyncLocked
            && instance.nested_compositor.is_some()
        {
            mark_vsync_sample_fence(&instance.gl);
        }
        instance.gl.flush();
    }

    // EXPERIMENTAL: SurfaceTexture quads on top of remote SHM (opt-in env only).
    #[cfg(target_os = "linux")]
    if hole_punch {
        for (pane_id, px, py, pw, ph, shm_seq) in hole_punch_panes {
            let _ = crate::gl::webgl_broker::draw_hole_punch_quads_for_pane(
                instance.rendering_context.as_ref(),
                &pane_id,
                shm_seq,
                |gl_tex, _tw, _th, rx, ry, rw, rh| {
                    let Some(nz) = std::num::NonZeroU32::new(gl_tex) else {
                        return;
                    };
                    let (qx, qy, qw, qh) = if rw > 0 && rh > 0 {
                        let qx = px + rx;
                        let qy = py + (ph - ry - rh);
                        let left = qx.max(px);
                        let right = (qx + rw).min(px + pw);
                        let bottom = qy.max(py);
                        let top = (qy + rh).min(py + ph);
                        let cw = right - left;
                        let ch = top - bottom;
                        if cw <= 0 || ch <= 0 {
                            return;
                        }
                        (left, bottom, cw, ch)
                    } else {
                        (px, py, pw, ph)
                    };
                    draw_pane_texture(
                        instance,
                        glow::NativeTexture(nz),
                        qx,
                        qy,
                        qw,
                        qh,
                        scale_factor,
                        0,
                    );
                },
            );
        }
    }

    CompositeRemoteResult {
        remotes_with_new_frames,
        cover_ready_panes,
    }
}

// Fences for the last two client-buffer samples on this GL thread.
// `.0` is the sample a present may retire (the buffer no longer on screen).
// `.1` is the sample just submitted, which stays pinned. A later fence is
// ordered after every earlier blit in the same context, so waiting on `.0`
// proves the retired buffer's GPU reads have finished.
#[cfg(target_os = "linux")]
thread_local! {
    static VSYNC_SAMPLE_FENCES: std::cell::Cell<(Option<glow::Fence>, Option<glow::Fence>)> =
        const { std::cell::Cell::new((None, None)) };
}

/// Insert a fence after the composite that sampled the current client buffer.
#[cfg(target_os = "linux")]
pub fn mark_vsync_sample_fence(gl: &glow::Context) {
    let fence = unsafe { gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0).ok() };
    let Some(fence) = fence.filter(|fence| !fence.0.is_null()) else {
        return;
    };
    VSYNC_SAMPLE_FENCES.with(|slot| {
        let (older, newer) = slot.get();
        if let Some(older) = older {
            unsafe { gl.delete_sync(older) };
        }
        slot.set((newer, Some(fence)));
    });
}

/// Wait until the previous client-buffer sample has retired.
///
/// `true` when there is no prior sample, or its fence has signaled. A timeout
/// means the buffer is still readable by the GPU and must stay pinned.
#[cfg(target_os = "linux")]
pub fn wait_retired_sample_fence(gl: &glow::Context) -> bool {
    let fence = VSYNC_SAMPLE_FENCES.with(|slot| slot.get().0);
    let Some(fence) = fence.filter(|fence| !fence.0.is_null()) else {
        return true;
    };
    const TIMEOUT_NS: i32 = 50_000_000;
    let status = unsafe {
        gl.client_wait_sync(fence, glow::SYNC_FLUSH_COMMANDS_BIT, TIMEOUT_NS)
    };
    let signaled = status == glow::ALREADY_SIGNALED || status == glow::CONDITION_SATISFIED;
    if !signaled {
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::warn!(
                "vsync sample fence did not signal (status {status:#x}); keeping the scanned-out wl_buffer pinned"
            );
        }
    }
    signaled
}

/// Host swap choke point. Returns `true` if `eglSwapBuffers` ran.
///
/// - VRR: eglSwapInterval(0), no phase sleep — outer adaptive-sync is the clock.
/// - Fixed desktop: eglSwapInterval(1) — compositor FIFO.
/// - Fixed gamescope: interval 0 (Xwayland cannot FIFO) + absolute-deadline
///   software FIFO phase-locked via DRM vblank UST (wp_presentation equivalent).
pub fn atrium_host_present(instance: &WindowInstance) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::Instant;

        static SWAP_COUNT: AtomicU64 = AtomicU64::new(0);
        static LAST_PRESENT_AT: Mutex<Option<Instant>> = Mutex::new(None);
        static SOFT_PACER: Mutex<Option<crate::pacing::SoftFifoPacer>> = Mutex::new(None);
        static PACING_DIAG: Mutex<Option<crate::pacing::PacingDiag>> = Mutex::new(None);
        static FEEDBACK_SEQ: Mutex<u64> = Mutex::new(0);

        let gamescope = crate::app::is_gamescope_session_pub();
        let activity = crate::pacing::present_activity(instance);
        let policy = instance.pacing_policy;
        let mhz = instance.refresh_rate_millihertz.max(1);
        let period_ns = 1_000_000_000_000u64 / mhz as u64;
        let period_ms = period_ns as f32 / 1_000_000.0;
        let pacing_diag = std::env::var_os("ATRIUM_PACING_DIAG").is_some();
        let soft_fifo = crate::pacing::fixed_software_fifo(instance);

        if crate::pacing::host_should_skip_blocking_present(instance) {
            return false;
        }

        // Policy-stable swap interval (cached — no per-frame Mesa round-trip).
        if crate::pacing::wants_immediate_swap(instance) {
            crate::app::nest_set_egl_swap_interval_pub(0);
        } else {
            crate::app::nest_set_egl_swap_interval_pub(1);
        }

        // Absolute CLOCK_MONOTONIC deadline grid + CRTC PLL (not relative post-swap sleep).
        let mut gate_ms = 0.0f32;
        let mut phase_err_ms = 0.0f32;
        let mut phase_slips = 0u64;
        if soft_fifo {
            let mut pacer_guard = SOFT_PACER.lock().unwrap_or_else(|e| e.into_inner());
            let pacer = pacer_guard.get_or_insert_with(|| crate::pacing::SoftFifoPacer::new(period_ns));
            pacer.set_period_ns(period_ns);
            phase_err_ms = pacer.last_phase_err_ns as f32 / 1_000_000.0;
            phase_slips = pacer.phase_slips;
            match pacer.wait_for_swap_slot() {
                crate::pacing::SoftFifoWait::Deferred { remaining_ms } => {
                    // Too early — leave the grid alone. WaitUntil + gate-due redraw
                    // will present on cadence; flipping now free-runs the host.
                    if pacing_diag {
                        static DEFER_LOG: AtomicU64 = AtomicU64::new(0);
                        let n = DEFER_LOG.fetch_add(1, Ordering::Relaxed) + 1;
                        if n <= 8 || n % 240 == 0 {
                            crate::app::nest_host_diag_pub(&format!(
                                "soft_fifo defer present (remain={remaining_ms:.2}ms) — WaitUntil owns gate"
                            ));
                        }
                    }
                    return false;
                }
                crate::pacing::SoftFifoWait::Ready { gate_ms: g } => {
                    gate_ms = g;
                }
            }
        } else {
            *SOFT_PACER.lock().unwrap_or_else(|e| e.into_inner()) = None;
            crate::pacing::clear_soft_fifo_gate_publish();
        }

        let hold = crate::pacing::wants_fixed_hold_cadence(instance);
        let vrr_primary = crate::pacing::vrr_requires_primary_damage(instance)
            && crate::pacing::primary_has_unpresented_frame(instance);
        if hold {
            crate::pacing::note_hold_present();
        }
        if vrr_primary {
            crate::pacing::note_vrr_primary_present();
        }

        let n = SWAP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if pacing_diag && n <= 8 {
            crate::app::nest_host_diag_pub(&format!(
                "pre-swap #{n} gamescope={gamescope} policy={} activity={} soft_fifo={} hold={} vrr_pri={} gate_ms={:.2} phase_err_ms={:.2} viewable={} focused={} period_ms={:.2}",
                crate::pacing::policy_label(policy),
                crate::pacing::activity_label(activity),
                soft_fifo,
                hold,
                vrr_primary,
                gate_ms,
                phase_err_ms,
                crate::pacing::window_is_viewable(instance),
                instance.os_focused,
                period_ms
            ));
        }

        let t0 = Instant::now();
        instance.rendering_context.present();
        let post_swap_ns = crate::pacing::clock_monotonic_ns();
        let swap_ms = t0.elapsed().as_secs_f32() * 1000.0;

        let mut drm_sample_ok = false;
        let mut pll_applied = false;
        if soft_fifo {
            // Gamescope X11: DRM vblank UST ≈ wp_presentation.presented. Apply PLL
            // against the deadline we just targeted, then advance the absolute grid.
            drm_sample_ok = crate::present_feedback::sample_drm_vblank_feedback(period_ns);
            let mut pacer_guard = SOFT_PACER.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pacer) = pacer_guard.as_mut() {
                let mut seq = FEEDBACK_SEQ.lock().unwrap_or_else(|e| e.into_inner());
                if let Some((t, refresh)) = crate::present_feedback::take_presented_since(&mut seq)
                {
                    pacer.apply_presented(t, refresh);
                    pll_applied = true;
                }
                pacer.advance_after_swap(post_swap_ns);
                phase_slips = pacer.phase_slips;
                phase_err_ms = pacer.last_phase_err_ns as f32 / 1_000_000.0;
            }
        }

        let present_dt_ms = {
            let now_inst = Instant::now();
            let mut prev = LAST_PRESENT_AT.lock().unwrap_or_else(|e| e.into_inner());
            let dt = prev.map(|p| now_inst.duration_since(p).as_secs_f32() * 1000.0);
            *prev = Some(now_inst);
            dt
        };
        if let Some(dt_ms) = present_dt_ms {
            record_present_stats(instance, dt_ms);
            if pacing_diag {
                let clocks = read_present_clocks(instance, mhz as f32 / 1000.0);
                let mut diag_guard = PACING_DIAG.lock().unwrap_or_else(|e| e.into_inner());
                let diag = diag_guard.get_or_insert_with(crate::pacing::PacingDiag::new);
                let stages = crate::pacing::take_frame_stages();
                if let Some(line) = diag.observe(
                    dt_ms,
                    swap_ms,
                    gate_ms,
                    phase_err_ms,
                    phase_slips,
                    period_ms,
                    stages,
                    soft_fifo,
                    drm_sample_ok,
                    pll_applied,
                    crate::pacing::policy_label(policy),
                    crate::pacing::activity_label(activity),
                    clocks,
                ) {
                    crate::app::nest_host_diag_pub(&line);
                }
            }
        }
        return true;
    }
    #[cfg(not(target_os = "linux"))]
    {
        instance.rendering_context.present();
        true
    }
}

/// Draw the scene texture into the current window backbuffer (no swap).
unsafe fn draw_scene_to_backbuffer(
    instance: &WindowInstance,
    live_w: i32,
    live_h: i32,
    scale_factor: f32,
) {
    let _ = instance.rendering_context.make_current();
    instance.rendering_context.prepare_for_rendering();

    instance.gl.disable(glow::SCISSOR_TEST);
    instance.gl.disable(glow::DEPTH_TEST);
    instance.gl.disable(glow::BLEND);
    instance.gl.viewport(0, 0, live_w, live_h);

    if let Some(program) = instance.comp_program {
        if let Some(tex) = instance.scene_tex {
            instance.gl.use_program(Some(program));
            instance.gl.bind_vertex_array(instance.comp_vao);
            instance.gl.active_texture(glow::TEXTURE0);
            instance.gl.bind_texture(glow::TEXTURE_2D, Some(tex));

            instance
                .gl
                .uniform_2_f32(instance.u_comp_size_loc.as_ref(), live_w as f32, live_h as f32);
            let radius = instance.corner_radius.unwrap_or(0.0) as f32 * scale_factor;
            instance
                .gl
                .uniform_1_f32(instance.u_comp_radius_loc.as_ref(), radius);
            instance.gl.uniform_1_i32(instance.u_comp_flip_loc.as_ref(), 0);

            instance.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            instance.gl.bind_vertex_array(None);
            instance.gl.use_program(None);
        }
    }
}

/// Blit scene FBO to the window and present. Returns whether the swapchain flipped.
pub fn blit_scene_to_window(
    instance: &mut WindowInstance,
    live_w: i32,
    live_h: i32,
    scale_factor: f32,
) -> bool {
    #[cfg(target_os = "linux")]
    let blit_t0 = std::env::var_os("ATRIUM_PACING_DIAG").map(|_| std::time::Instant::now());
    unsafe {
        draw_scene_to_backbuffer(instance, live_w, live_h, scale_factor);

        #[cfg(target_os = "linux")]
        if let Some(t0) = blit_t0 {
            // Composite time was stashed in app.rs; fold window blit into draw_ms
            // before present consumes the stage bag.
            crate::pacing::bump_frame_draw_ms(t0.elapsed().as_secs_f32() * 1000.0);
        }
        // One swap per SoftFifo/VRR present. A duplicate "mailbox seal" swap made
        // gamescope report 2× rate (60→120) while atrium still showed 60fps.
        atrium_host_present(instance)
    }
}

#[cfg(target_os = "linux")]
fn read_present_clocks(
    instance: &WindowInstance,
    refresh_hz: f32,
) -> crate::pacing::PresentClockSnap {
    let mut snap = crate::pacing::PresentClockSnap {
        refresh_hz,
        ..Default::default()
    };
    let Some(game_id) = instance.primary_game_pane.as_ref() else {
        return snap;
    };
    let Some(pane) = instance.panes.get(game_id) else {
        return snap;
    };
    let Some(slot) = pane.external_surface_slot.as_ref() else {
        return snap;
    };
    let guard = slot.lock();
    snap.ingest_fps = guard.ingest_fps;
    snap.present_fps = guard.present_fps;
    snap.commits_per_swap = guard.last_commits_per_swap;
    snap
}

#[cfg(target_os = "linux")]
fn record_present_stats(instance: &WindowInstance, present_dt_ms: f32) {
    let Some(game_id) = instance.primary_game_pane.as_ref() else {
        return;
    };
    let Some(pane) = instance.panes.get(game_id) else {
        return;
    };
    let Some(slot) = pane.external_surface_slot.as_ref() else {
        return;
    };
    let refresh_mhz = instance.refresh_rate_millihertz.max(1);
    let refresh_hz = refresh_mhz as f32 / 1000.0;
    let min_ft_ms = 1000.0 / refresh_hz.max(1.0);
    let ft = present_dt_ms.max(min_ft_ms * 0.5);
    let mut guard = slot.lock();
    let coalesced = guard.ingest_since_present.max(1) as f32;
    guard.ingest_since_present = 0;
    guard.last_commits_per_swap = if guard.last_commits_per_swap == 0.0 {
        coalesced
    } else {
        guard.last_commits_per_swap * 0.8 + coalesced * 0.2
    };
    guard.present_counter = guard.present_counter.wrapping_add(1);
    guard.last_present_instant = Some(std::time::Instant::now());
    guard.rolling_present_ms.push(ft);
    if guard.rolling_present_ms.len() > 60 {
        guard.rolling_present_ms.remove(0);
    }
    let instant_fps = (1000.0 / ft).min(refresh_hz * 1.05);
    if guard.present_fps == 0.0 {
        guard.present_fps = instant_fps;
    } else {
        guard.present_fps = guard.present_fps * 0.85 + instant_fps * 0.15;
    }
    guard.current_fps = guard.present_fps;
    guard.last_frame_time_ms = ft;
    guard.rolling_frametimes.push(ft);
    if guard.rolling_frametimes.len() > 60 {
        guard.rolling_frametimes.remove(0);
    }
    guard.stats_updated = true;
}
