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

pub fn composite_panes_to_scene(
    instance: &mut WindowInstance,
    active_w: u32,
    active_h: u32,
    scale_factor: f32,
    is_transaction_pending: bool,
    transaction_complete: bool,
    pending_hides: &HashSet<String>,
) {
    unsafe {
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

        let current_safe_size = winit::dpi::PhysicalSize::new(active_w, active_h);
        let mut sorted_panes: Vec<_> = instance.panes.values_mut().collect();
        sorted_panes.sort_by_key(|p| p.z_index);

        for pane in sorted_panes.iter_mut() {
            if !pane.is_visible { continue; }

            let (layout_pw, layout_ph, layout_px, layout_py) = {
                let x_start = pane.last_physical_rect.origin.x;
                let y_start = pane.last_physical_rect.origin.y;
                let layout_pw = pane.last_physical_rect.size.width;
                let layout_ph = pane.last_physical_rect.size.height;

                let layout_py = active_h as i32 - (y_start + layout_ph);
                (layout_pw, layout_ph, x_start, layout_py)
            };

            let tex_size = pane.offscreen_ctx.size();
            let tex_pw = tex_size.width as i32;
            let tex_ph = tex_size.height as i32;

            if layout_pw <= 0 || layout_ph <= 0 || tex_pw <= 0 || tex_ph <= 0 { continue; }

            let is_locked = is_transaction_pending && !transaction_complete;
            let is_hiding = pending_hides.contains(&pane.id);

            let freeze_this_pane = (is_locked || pane.is_resizing || is_hiding) && pane.ghost_tex_size.is_some();

            if let Some(callback) = pane.offscreen_ctx.render_to_parent_callback() {
                if let (Some(comp_fbo), Some(comp_tex)) = (instance.comp_fbo, instance.comp_tex) {
                    if pane.ghost_tex.is_none() {
                        pane.ghost_tex = Some(instance.gl.create_texture().unwrap());
                    }

                    if freeze_this_pane {
                        instance.gl.bind_framebuffer(glow::FRAMEBUFFER, instance.scene_fbo);
                        instance.gl.viewport(0, 0, active_w as i32, active_h as i32);
                        instance.gl.enable(glow::BLEND);
                        instance.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);

                        if let Some(program) = instance.comp_program {
                            instance.gl.use_program(Some(program));
                            instance.gl.bind_vertex_array(instance.comp_vao);
                            instance.gl.active_texture(glow::TEXTURE0);
                            instance.gl.bind_texture(glow::TEXTURE_2D, pane.ghost_tex);

                            instance.gl.uniform_2_f32(instance.u_comp_size_loc.as_ref(), active_w as f32, active_h as f32);
                            let radius = instance.corner_radius.unwrap_or(0.0) as f32 * scale_factor;
                            instance.gl.uniform_1_f32(instance.u_comp_radius_loc.as_ref(), radius);

                            instance.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                            instance.gl.bind_vertex_array(None);
                        }
                    } else {
                        instance.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(comp_fbo));
                        instance.gl.bind_texture(glow::TEXTURE_2D, Some(comp_tex));
                        instance.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                        instance.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                        instance.gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(comp_tex), 0);
                        instance.gl.clear_color(0.0, 0.0, 0.0, 0.0);
                        instance.gl.clear(glow::COLOR_BUFFER_BIT);

                        let target_rect = euclid::default::Rect::new(
                            euclid::default::Point2D::new(layout_px, layout_py),
                            euclid::default::Size2D::new(layout_pw, layout_ph)
                        );
                        callback(&instance.gl, target_rect, comp_fbo.0.get());

                        instance.gl.bind_texture(glow::TEXTURE_2D, pane.ghost_tex);
                        if pane.ghost_tex_size != Some(current_safe_size) {
                            instance.gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, active_w as i32, active_h as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
                            instance.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                            instance.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                            pane.ghost_tex_size = Some(current_safe_size);
                        }
                        instance.gl.copy_tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, 0, 0, active_w as i32, active_h as i32);

                        instance.gl.bind_framebuffer(glow::FRAMEBUFFER, instance.scene_fbo);
                        instance.gl.disable(glow::SCISSOR_TEST);
                        instance.gl.disable(glow::DEPTH_TEST);
                        instance.gl.viewport(0, 0, active_w as i32, active_h as i32);
                        instance.gl.enable(glow::BLEND);
                        instance.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);

                        if let Some(program) = instance.comp_program {
                            instance.gl.use_program(Some(program));
                            instance.gl.bind_vertex_array(instance.comp_vao);
                            instance.gl.active_texture(glow::TEXTURE0);
                            instance.gl.bind_texture(glow::TEXTURE_2D, Some(comp_tex));

                            instance.gl.uniform_2_f32(instance.u_comp_size_loc.as_ref(), active_w as f32, active_h as f32);
                            let radius = instance.corner_radius.unwrap_or(0.0) as f32 * scale_factor;
                            instance.gl.uniform_1_f32(instance.u_comp_radius_loc.as_ref(), radius);
                            instance.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                            instance.gl.bind_vertex_array(None);
                        }
                    }
                }
            }
        }

        instance.gl.color_mask(true, true, true, true);
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

                instance.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                instance.gl.bind_vertex_array(None);
                instance.gl.use_program(None);
            }
        }

        instance.rendering_context.present();
    }
}
