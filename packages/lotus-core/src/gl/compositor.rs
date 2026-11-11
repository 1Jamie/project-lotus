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
    draw_pane_texture_ext(
        instance,
        tex,
        glow::TEXTURE_2D,
        layout_px,
        layout_py,
        layout_pw,
        layout_ph,
        scale_factor,
        flip_y,
        false,
    );
}

/// GL_TEXTURE_EXTERNAL_OES — must bind with this target, not TEXTURE_2D.
const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

fn draw_pane_texture_ext(
    instance: &WindowInstance,
    tex: glow::NativeTexture,
    target: u32,
    layout_px: i32,
    layout_py: i32,
    layout_pw: i32,
    layout_ph: i32,
    scale_factor: f32,
    flip_y: i32,
    opaque: bool,
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
    );
}

/// Like [`draw_pane_texture_ext`], but viewport (draw dest) may differ from scissor
/// (clip). Used for cover-fit overlays: draw a larger quad, clip to the dock pane.
/// `opaque`: primary/game fullscreen blit — skip blending for a pure 1:1 copy.
fn draw_pane_texture_ext_clipped(
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
    scale_factor: f32,
    flip_y: i32,
    opaque: bool,
) {
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

        let use_oes = target == GL_TEXTURE_EXTERNAL_OES;
        let (program, size_loc, radius_loc, flip_loc) = if use_oes {
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

        if let Some(program) = program {
            instance.gl.use_program(Some(program));
            instance.gl.bind_vertex_array(instance.comp_vao);
            instance.gl.active_texture(glow::TEXTURE0);
            // External OES textures must be bound to TEXTURE_EXTERNAL_OES, not TEXTURE_2D.
            instance.gl.bind_texture(target, Some(tex));

            instance
                .gl
                .uniform_2_f32(size_loc, viewport_pw as f32, viewport_ph as f32);
            // Corner radius follows the visible clip (dock), not the oversized cover quad.
            let radius = instance.corner_radius.unwrap_or(0.0) as f32 * scale_factor;
            instance.gl.uniform_1_f32(radius_loc, radius);
            instance.gl.uniform_1_i32(flip_loc, flip_y);

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
        let skip_clear = instance.game_mode
            && instance.primary_game_pane.as_ref().is_some_and(|pid| {
                instance
                    .panes
                    .get(pid)
                    .and_then(|p| p.external_surface_slot.as_ref())
                    .is_some_and(|s| s.lock().current_texture.is_some())
            });
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
                    );
                }
            }
        }

        instance.gl.color_mask(true, true, true, true);
        instance.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        instance.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        // Push the primary blit to the GPU before swap so we can release the game's
        // wl_buffer immediately after present without waiting another frame.
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

/// Host swap choke point. Returns `true` if `eglSwapBuffers` ran.
///
/// Game-driven VRR passthrough (nested atrium):
/// - gamescope / game-driven: eglSwapInterval(0), no software phase sleep — outer
///   adaptive-sync pageflip is the clock; FrameDone follows this swap.
/// - otherwise: eglSwapInterval(1) — host compositor FIFO.
pub fn atrium_host_present(instance: &WindowInstance) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::Instant;

        static SWAP_COUNT: AtomicU64 = AtomicU64::new(0);
        static LAST_PRESENT_AT: Mutex<Option<Instant>> = Mutex::new(None);

        let gamescope = crate::app::is_gamescope_session_pub();
        let game_driven = crate::app::atrium_game_driven_present(instance);
        let mhz = instance.refresh_rate_millihertz.max(1);
        let min_period_ms = 1_000_000.0f32 / mhz as f32;
        let pacing_diag = std::env::var_os("ATRIUM_PACING_DIAG").is_some();

        // Never phase-sleep here — that imposes a fixed timer and kills VRR.
        // gamescope FIFO deadlocks with interval 1; game-driven needs interval 0 too.
        if gamescope || game_driven {
            crate::app::atrium_set_egl_swap_interval_pub(0);
        } else {
            crate::app::atrium_set_egl_swap_interval_pub(1);
        }

        let n = SWAP_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if pacing_diag && (n <= 8 || n % 120 == 0) {
            crate::app::atrium_host_diag_pub(&format!(
                "pre-swap #{n} gamescope={gamescope} game_driven={game_driven} focused={} min_period_ms={:.2}",
                instance.os_focused, min_period_ms
            ));
        }
        let t0 = Instant::now();
        instance.rendering_context.present();
        let now = Instant::now();
        let swap_ms = t0.elapsed().as_secs_f32() * 1000.0;
        let present_dt_ms = {
            let mut prev = LAST_PRESENT_AT.lock().unwrap_or_else(|e| e.into_inner());
            let dt = prev.map(|p| now.duration_since(p).as_secs_f32() * 1000.0);
            *prev = Some(now);
            dt
        };
        if pacing_diag
            && (n <= 8
                || n % 120 == 0
                || swap_ms > 50.0
                || present_dt_ms.map(|d| d > 50.0).unwrap_or(false))
        {
            crate::app::atrium_host_diag_pub(&format!(
                "post-swap #{n} swap={swap_ms:.2}ms present_dt={} focused={} occluded={}",
                present_dt_ms
                    .map(|d| format!("{d:.2}ms"))
                    .unwrap_or_else(|| "n/a".into()),
                instance.os_focused,
                instance.os_occluded
            ));
        }
        return true;
    }
    #[cfg(not(target_os = "linux"))]
    {
        instance.rendering_context.present();
        true
    }
}

/// Blit scene FBO to the window and present. Returns whether the swapchain flipped.
pub fn blit_scene_to_window(
    instance: &mut WindowInstance,
    live_w: i32,
    live_h: i32,
    scale_factor: f32,
) -> bool {
    unsafe {
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

                instance.gl.uniform_2_f32(instance.u_comp_size_loc.as_ref(), live_w as f32, live_h as f32);
                let radius = instance.corner_radius.unwrap_or(0.0) as f32 * scale_factor;
                instance.gl.uniform_1_f32(instance.u_comp_radius_loc.as_ref(), radius);
                instance.gl.uniform_1_i32(instance.u_comp_flip_loc.as_ref(), 0);

                instance.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                instance.gl.bind_vertex_array(None);
                instance.gl.use_program(None);
            }
        }

        atrium_host_present(instance)
    }
}
