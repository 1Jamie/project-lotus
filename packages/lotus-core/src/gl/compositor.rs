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
    unsafe {
        instance.gl.bind_framebuffer(glow::FRAMEBUFFER, instance.scene_fbo);
        instance.gl.viewport(layout_px, layout_py, layout_pw, layout_ph);
        instance.gl.enable(glow::SCISSOR_TEST);
        instance.gl.scissor(layout_px, layout_py, layout_pw, layout_ph);
        instance.gl.enable(glow::BLEND);
        instance.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);

        if let Some(program) = instance.comp_program {
            instance.gl.use_program(Some(program));
            instance.gl.bind_vertex_array(instance.comp_vao);
            instance.gl.active_texture(glow::TEXTURE0);
            instance.gl.bind_texture(glow::TEXTURE_2D, Some(tex));

            instance.gl.uniform_2_f32(instance.u_comp_size_loc.as_ref(), layout_pw as f32, layout_ph as f32);
            let radius = instance.corner_radius.unwrap_or(0.0) as f32 * scale_factor;
            instance.gl.uniform_1_f32(instance.u_comp_radius_loc.as_ref(), radius);
            instance.gl.uniform_1_i32(instance.u_comp_flip_loc.as_ref(), flip_y);

            instance.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            instance.gl.bind_vertex_array(None);
            instance.gl.use_program(None);
        }
        instance.gl.disable(glow::SCISSOR_TEST);
    }
}

/// Capture pane-sized last-good pixels into `ghost_tex` for stretch-interim during resize.
fn capture_local_ghost(instance: &mut WindowInstance, pane_id: &str) {
    let Some(pane) = instance.panes.get_mut(pane_id) else {
        return;
    };
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
    }
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
    unsafe {
        // Defensive: never let a prior remote upload leave UNPACK_* set for Servo/WebRender.
        instance.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        instance.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        instance.gl.bind_framebuffer(glow::FRAMEBUFFER, instance.scene_fbo);
        instance.gl.disable(glow::SCISSOR_TEST);
        instance.gl.viewport(0, 0, active_w as i32, active_h as i32);

        instance.gl.color_mask(true, true, true, true);
        if instance.transparent {
            instance.gl.clear_color(0.0, 0.0, 0.0, 0.0);
        } else {
            instance.gl.clear_color(0.0, 0.0, 0.0, 1.0);
        }
        instance.gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT | glow::STENCIL_BUFFER_BIT);

        // Prefer pending rect for same-window reflows (weights/dock/gutter) so the
        // slot snaps while the old texture stretches until nuclear flip.
        // Skip when staged was solved for a different window size than the active
        // scene — otherwise downsize jumps ahead and leaves black bars until flip.
        let pending_fits_active = instance.staged_layout.width == active_w
            && instance.staged_layout.height == active_h;

        let mut sorted_ids: Vec<_> = instance.panes.keys().cloned().collect();
        sorted_ids.sort_by_key(|id| instance.panes.get(id).map(|p| p.z_index).unwrap_or(0));

        for id in sorted_ids {
            enum DrawPlan {
                Skip,
                Remote { tex: glow::NativeTexture },
                LocalFreeze { tex: glow::NativeTexture },
                LocalLive { tex: glow::NativeTexture, capture: bool },
            }

            let (layout_px, layout_py, layout_pw, layout_ph, plan) = {
                let Some(pane) = instance.panes.get(&id) else {
                    continue;
                };
                if !pane.is_visible {
                    continue;
                }
                // Same-window interim: always use staged_layout for EVERY pane so
                // neighbors cannot diverge (pending armed on one, last_physical on
                // another → black seam). Textures stretch into these slots.
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
                    pane.last_physical_rect
                };
                if rect.size.width <= 0 || rect.size.height <= 0 {
                    continue;
                }
                let layout_pw = rect.size.width;
                let layout_ph = rect.size.height;
                let layout_px = rect.origin.x;
                let layout_py = active_h as i32 - (rect.origin.y + layout_ph);

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
                    let freeze = (is_locked || pane.is_resizing || is_hiding)
                        && pane.ghost_tex_size.is_some();
                    let tex_size = pane.offscreen_ctx.size();
                    let live_ok = tex_size.width > 0 && tex_size.height > 0;
                    if freeze {
                        if let Some(tex) = pane.ghost_tex {
                            DrawPlan::LocalFreeze { tex }
                        } else if live_ok {
                            // No ghost yet — still fill the slot (avoid black hole).
                            DrawPlan::LocalLive {
                                tex: pane.offscreen_ctx.color_texture(),
                                capture: true,
                            }
                        } else {
                            DrawPlan::Skip
                        }
                    } else if live_ok {
                        DrawPlan::LocalLive {
                            tex: pane.offscreen_ctx.color_texture(),
                            capture: true,
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
            }
        }

        instance.gl.color_mask(true, true, true, true);
        instance.gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        instance.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
    }
    CompositeRemoteResult {
        remotes_with_new_frames,
        cover_ready_panes,
    }
}

pub fn blit_scene_to_window(
    instance: &mut WindowInstance,
    live_w: i32,
    live_h: i32,
    scale_factor: f32,
) {
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

        instance.rendering_context.present();
    }
}
