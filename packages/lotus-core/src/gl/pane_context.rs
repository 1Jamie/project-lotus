//! Per-pane framebuffer on the window GL context.
//!
//! WebRender paints into this framebuffer. The compositor blits it into the
//! scene target. This replaces `OffscreenRenderingContext::render_to_parent_callback`,
//! which could only blit to the window framebuffer without a Servo patch.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use dpi::PhysicalSize;
use glow::HasContext;
use servo::{DeviceIntRect, RenderingContext, RgbaImage, WindowRenderingContext};

struct PaneFramebuffer {
    fbo: glow::NativeFramebuffer,
    texture: glow::NativeTexture,
    depth: glow::NativeRenderbuffer,
}

impl PaneFramebuffer {
    fn new(gl: &glow::Context, size: PhysicalSize<u32>) -> Self {
        unsafe {
            let fbo = gl.create_framebuffer().expect("LotusPaneContext: create framebuffer");
            let texture = gl.create_texture().expect("LotusPaneContext: create texture");
            let depth = gl.create_renderbuffer().expect("LotusPaneContext: create depth buffer");

            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                size.width as i32,
                size.height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );

            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(depth));
            gl.renderbuffer_storage(
                glow::RENDERBUFFER,
                glow::DEPTH_COMPONENT24,
                size.width as i32,
                size.height as i32,
            );
            gl.framebuffer_renderbuffer(
                glow::FRAMEBUFFER,
                glow::DEPTH_ATTACHMENT,
                glow::RENDERBUFFER,
                Some(depth),
            );

            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.bind_renderbuffer(glow::RENDERBUFFER, None);

            Self { fbo, texture, depth }
        }
    }

    fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.delete_renderbuffer(self.depth);
            gl.delete_texture(self.texture);
            gl.delete_framebuffer(self.fbo);
        }
    }
}

pub struct LotusPaneContext {
    parent: Rc<WindowRenderingContext>,
    glow: Arc<glow::Context>,
    size: Cell<PhysicalSize<u32>>,
    framebuffer: RefCell<PaneFramebuffer>,
}

impl LotusPaneContext {
    pub fn new(parent: Rc<WindowRenderingContext>, size: PhysicalSize<u32>) -> Self {
        assert!(
            size.width > 0 && size.height > 0,
            "LotusPaneContext dimensions must be at least 1x1, got {size:?}",
        );
        parent
            .make_current()
            .expect("LotusPaneContext: make_current failed");
        let glow = parent.glow_gl_api();
        let framebuffer = PaneFramebuffer::new(&glow, size);
        Self {
            parent,
            glow,
            size: Cell::new(size),
            framebuffer: RefCell::new(framebuffer),
        }
    }

    pub fn color_texture(&self) -> glow::NativeTexture {
        self.framebuffer.borrow().texture
    }

    pub fn bind_color_for_read(&self, gl: &glow::Context) {
        unsafe {
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(self.framebuffer.borrow().fbo));
        }
    }

    /// Blit this pane's color buffer into `target` at `target_rect`.
    ///
    /// Leaves `target` bound to `GL_FRAMEBUFFER`, matching the previous
    /// `render_to_parent_callback` so the ghost-texture copy still reads it.
    pub fn blit_into(
        &self,
        gl: &glow::Context,
        target: glow::NativeFramebuffer,
        target_rect: euclid::default::Rect<i32>,
    ) {
        let size = self.size.get();
        let source = self.framebuffer.borrow().fbo;
        unsafe {
            gl.scissor(
                target_rect.origin.x,
                target_rect.origin.y,
                target_rect.size.width,
                target_rect.size.height,
            );
            gl.enable(glow::SCISSOR_TEST);
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.disable(glow::SCISSOR_TEST);

            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(source));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(target));
            gl.blit_framebuffer(
                0,
                0,
                size.width as i32,
                size.height as i32,
                target_rect.origin.x,
                target_rect.origin.y,
                target_rect.origin.x + target_rect.size.width,
                target_rect.origin.y + target_rect.size.height,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target));
        }
    }
}

impl Drop for LotusPaneContext {
    fn drop(&mut self) {
        let _ = self.parent.make_current();
        self.framebuffer.borrow().destroy(&self.glow);
    }
}

impl RenderingContext for LotusPaneContext {
    fn prepare_for_rendering(&self) {
        let fbo = self.framebuffer.borrow().fbo;
        unsafe {
            self.glow.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        }
    }

    fn read_to_image(&self, source_rectangle: DeviceIntRect) -> Option<RgbaImage> {
        let width = source_rectangle.width();
        let height = source_rectangle.height();
        if width <= 0 || height <= 0 {
            return None;
        }

        let gl = self.gleam_gl_api();
        let fbo = self.framebuffer.borrow().fbo.0.get();
        gl.bind_framebuffer(gleam::gl::FRAMEBUFFER, fbo);
        gl.bind_vertex_array(0);

        let mut pixels = gl.read_pixels(
            source_rectangle.min.x,
            source_rectangle.min.y,
            width,
            height,
            gleam::gl::RGBA,
            gleam::gl::UNSIGNED_BYTE,
        );

        let stride = width as usize * 4;
        let height = height as usize;
        let orig = pixels.clone();
        for y in 0..height {
            let dst = y * stride;
            let src = (height - y - 1) * stride;
            pixels[dst..dst + stride].copy_from_slice(&orig[src..src + stride]);
        }

        RgbaImage::from_raw(width as u32, height as u32, pixels)
    }

    fn size(&self) -> PhysicalSize<u32> {
        self.size.get()
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        assert!(
            size.width > 0 && size.height > 0,
            "LotusPaneContext dimensions must be at least 1x1, got {size:?}",
        );
        if self.size.get() == size {
            return;
        }

        let _ = self.make_current();
        let next = PaneFramebuffer::new(&self.glow, size);
        let old_size = self.size.get();
        let blit_w = old_size.width.min(size.width) as i32;
        let blit_h = old_size.height.min(size.height) as i32;
        unsafe {
            let old = self.framebuffer.borrow().fbo;
            self.glow.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(old));
            self.glow.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(next.fbo));
            self.glow.blit_framebuffer(
                0,
                0,
                blit_w,
                blit_h,
                0,
                0,
                blit_w,
                blit_h,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
        }

        let previous = self.framebuffer.replace(next);
        previous.destroy(&self.glow);
        self.size.set(size);
    }

    fn present(&self) {}

    fn make_current(&self) -> Result<(), surfman::Error> {
        self.parent.make_current()
    }

    fn gleam_gl_api(&self) -> Rc<dyn gleam::gl::Gl> {
        self.parent.gleam_gl_api()
    }

    fn glow_gl_api(&self) -> Arc<glow::Context> {
        self.glow.clone()
    }

    fn create_texture(
        &self,
        surface: surfman::Surface,
    ) -> Option<(surfman::SurfaceTexture, u32, euclid::default::Size2D<i32>)> {
        self.parent.create_texture(surface)
    }

    fn destroy_texture(&self, surface_texture: surfman::SurfaceTexture) -> Option<surfman::Surface> {
        self.parent.destroy_texture(surface_texture)
    }

    fn connection(&self) -> Option<surfman::Connection> {
        self.parent.connection()
    }

    fn refresh_driver(&self) -> Option<Rc<dyn servo::RefreshDriver>> {
        self.parent.refresh_driver()
    }
}
