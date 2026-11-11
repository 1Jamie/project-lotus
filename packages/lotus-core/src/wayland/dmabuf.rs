//! DMA-BUF acquisition, frame descriptors, and EGL zero-copy import.

use std::ffi::c_void;
use std::os::unix::io::AsRawFd;
use glow::HasContext;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::Buffer;

pub const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

// EGL extension definitions
pub const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
pub const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
pub const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
pub const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
pub const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
pub const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
pub const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;

pub const EGL_DMA_BUF_PLANE1_FD_EXT: i32 = 0x3275;
pub const EGL_DMA_BUF_PLANE1_OFFSET_EXT: i32 = 0x3276;
pub const EGL_DMA_BUF_PLANE1_PITCH_EXT: i32 = 0x3277;
pub const EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT: i32 = 0x3445;
pub const EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT: i32 = 0x3446;

pub const EGL_DMA_BUF_PLANE2_FD_EXT: i32 = 0x3278;
pub const EGL_DMA_BUF_PLANE2_OFFSET_EXT: i32 = 0x3279;
pub const EGL_DMA_BUF_PLANE2_PITCH_EXT: i32 = 0x327A;
pub const EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT: i32 = 0x3447;
pub const EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT: i32 = 0x3448;

pub const EGL_DMA_BUF_PLANE3_FD_EXT: i32 = 0x3440;
pub const EGL_DMA_BUF_PLANE3_OFFSET_EXT: i32 = 0x3441;
pub const EGL_DMA_BUF_PLANE3_PITCH_EXT: i32 = 0x3442;
pub const EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT: i32 = 0x3449;
pub const EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT: i32 = 0x344A;

pub const EGL_WIDTH: i32 = 0x3057;
pub const EGL_HEIGHT: i32 = 0x3056;
pub const EGL_NONE: i32 = 0x3038;
pub const EGL_NO_CONTEXT: *mut c_void = std::ptr::null_mut();

type EglCreateImageKHR = unsafe extern "C" fn(
    dpy: *mut c_void,
    ctx: *mut c_void,
    target: u32,
    buffer: *mut c_void,
    attrib_list: *const i32,
) -> *mut c_void;

type EglDestroyImageKHR = unsafe extern "C" fn(
    dpy: *mut c_void,
    image: *mut c_void,
) -> u32;

type GlEGLImageTargetTexture2DOES = unsafe extern "C" fn(
    target: u32,
    image: *const c_void,
);

type EglGetCurrentDisplay = unsafe extern "C" fn() -> *mut c_void;
type EglGetProcAddress = unsafe extern "C" fn(procname: *const i8) -> *mut c_void;

#[derive(Debug, Clone)]
pub struct DmabufFrameDescriptor {
    pub surface_id: u32,
    pub dmabuf: Dmabuf,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub has_modifier: bool,
    pub y_inverted: bool,
}

impl DmabufFrameDescriptor {
    pub fn new(surface_id: u32, dmabuf: Dmabuf) -> Self {
        let size = dmabuf.size();
        let format = dmabuf.format();
        let modifier: u64 = format.modifier.into();
        Self {
            surface_id,
            width: size.w.max(1) as u32,
            height: size.h.max(1) as u32,
            fourcc: format.code as u32,
            modifier,
            has_modifier: dmabuf.has_modifier(),
            y_inverted: dmabuf.y_inverted(),
            dmabuf,
        }
    }
}

pub struct ImportedTexture {
    pub texture: glow::Texture,
    pub target: u32,
    pub is_external_oes: bool,
    pub width: u32,
    pub height: u32,
    pub y_inverted: bool,
    pub egl_image: *mut c_void,
}

pub struct EglDmaBufImporter {
    egl_display: *mut c_void,
    p_create_image: Option<EglCreateImageKHR>,
    p_destroy_image: Option<EglDestroyImageKHR>,
    p_target_texture: Option<GlEGLImageTargetTexture2DOES>,
}

unsafe impl Send for EglDmaBufImporter {}

impl EglDmaBufImporter {
    pub fn new() -> Result<Self, &'static str> {
        unsafe {
            let lib = libc::dlopen(b"libEGL.so.1\0".as_ptr() as *const _, libc::RTLD_NOW);
            let lib = if lib.is_null() {
                libc::dlopen(b"libEGL.so\0".as_ptr() as *const _, libc::RTLD_NOW)
            } else {
                lib
            };
            if lib.is_null() {
                return Err("Failed to load libEGL.so");
            }

            let get_proc_sym = libc::dlsym(lib, b"eglGetProcAddress\0".as_ptr() as *const _);
            if get_proc_sym.is_null() {
                return Err("Failed to find eglGetProcAddress");
            }
            let egl_get_proc: EglGetProcAddress = std::mem::transmute(get_proc_sym);

            let get_display_sym = libc::dlsym(lib, b"eglGetCurrentDisplay\0".as_ptr() as *const _);
            let egl_display = if !get_display_sym.is_null() {
                let egl_get_current_display: EglGetCurrentDisplay = std::mem::transmute(get_display_sym);
                egl_get_current_display()
            } else {
                std::ptr::null_mut()
            };

            let load_proc = |name: &[u8]| -> *mut c_void {
                let p = egl_get_proc(name.as_ptr() as *const _);
                if !p.is_null() {
                    return p;
                }
                libc::dlsym(lib, name.as_ptr() as *const _)
            };

            let p_create_image: Option<EglCreateImageKHR> = {
                let p = load_proc(b"eglCreateImageKHR\0");
                if p.is_null() { None } else { Some(std::mem::transmute(p)) }
            };

            let p_destroy_image: Option<EglDestroyImageKHR> = {
                let p = load_proc(b"eglDestroyImageKHR\0");
                if p.is_null() { None } else { Some(std::mem::transmute(p)) }
            };

            let p_target_texture: Option<GlEGLImageTargetTexture2DOES> = {
                let p = load_proc(b"glEGLImageTargetTexture2DOES\0");
                if p.is_null() { None } else { Some(std::mem::transmute(p)) }
            };

            if p_create_image.is_none() || p_target_texture.is_none() {
                return Err("Required EGLImageKHR or glEGLImageTargetTexture2DOES extension missing");
            }

            Ok(Self {
                egl_display,
                p_create_image,
                p_destroy_image,
                p_target_texture,
            })
        }
    }

    /// Set or update the current active EGLDisplay if queried after GL context creation.
    pub fn ensure_display(&mut self) -> bool {
        if !self.egl_display.is_null() {
            return true;
        }
        unsafe {
            let lib = libc::dlopen(b"libEGL.so.1\0".as_ptr() as *const _, libc::RTLD_NOLOAD | libc::RTLD_NOW);
            if lib.is_null() {
                return false;
            }
            let sym = libc::dlsym(lib, b"eglGetCurrentDisplay\0".as_ptr() as *const _);
            if !sym.is_null() {
                let f: EglGetCurrentDisplay = std::mem::transmute(sym);
                self.egl_display = f();
            }
            !self.egl_display.is_null()
        }
    }

    /// Import a DMA-BUF descriptor into an OpenGL texture on the current GL context thread.
    pub fn import_texture(
        &mut self,
        gl: &glow::Context,
        desc: &DmabufFrameDescriptor,
    ) -> Result<ImportedTexture, &'static str> {
        if !self.ensure_display() {
            return Err("EGLDisplay is NULL (no active GL/EGL context)");
        }
        let create_image = self.p_create_image.ok_or("eglCreateImageKHR missing")?;
        let target_texture = self.p_target_texture.ok_or("glEGLImageTargetTexture2DOES missing")?;

        let num_planes = desc.dmabuf.num_planes();
        if num_planes == 0 {
            return Err("Dmabuf has 0 planes");
        }

        let mut attribs = Vec::with_capacity(32);
        attribs.push(EGL_WIDTH);
        attribs.push(desc.width as i32);
        attribs.push(EGL_HEIGHT);
        attribs.push(desc.height as i32);
        attribs.push(EGL_LINUX_DRM_FOURCC_EXT);
        attribs.push(desc.fourcc as i32);

        let plane_fds: Vec<_> = desc.dmabuf.handles().collect();
        let plane_offsets: Vec<_> = desc.dmabuf.offsets().collect();
        let plane_strides: Vec<_> = desc.dmabuf.strides().collect();

        let plane_keys = [
            (EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGL_DMA_BUF_PLANE0_PITCH_EXT, EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT),
            (EGL_DMA_BUF_PLANE1_FD_EXT, EGL_DMA_BUF_PLANE1_OFFSET_EXT, EGL_DMA_BUF_PLANE1_PITCH_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT),
            (EGL_DMA_BUF_PLANE2_FD_EXT, EGL_DMA_BUF_PLANE2_OFFSET_EXT, EGL_DMA_BUF_PLANE2_PITCH_EXT, EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT),
            (EGL_DMA_BUF_PLANE3_FD_EXT, EGL_DMA_BUF_PLANE3_OFFSET_EXT, EGL_DMA_BUF_PLANE3_PITCH_EXT, EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT),
        ];

        for i in 0..num_planes.min(4) {
            let (k_fd, k_off, k_pitch, k_mod_lo, k_mod_hi) = plane_keys[i];
            attribs.push(k_fd);
            attribs.push(plane_fds[i].as_raw_fd());
            attribs.push(k_off);
            attribs.push(plane_offsets[i] as i32);
            attribs.push(k_pitch);
            attribs.push(plane_strides[i] as i32);

            if desc.has_modifier {
                attribs.push(k_mod_lo);
                attribs.push((desc.modifier & 0xFFFFFFFF) as i32);
                attribs.push(k_mod_hi);
                attribs.push(((desc.modifier >> 32) & 0xFFFFFFFF) as i32);
            }
        }

        attribs.push(EGL_NONE);

        let image = unsafe {
            create_image(
                self.egl_display,
                EGL_NO_CONTEXT,
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attribs.as_ptr(),
            )
        };

        if image.is_null() {
            return Err("eglCreateImageKHR returned EGL_NO_IMAGE_KHR");
        }

        // Host compose is desktop GL (#version 330) — Mesa rejects
        // GL_OES_EGL_image_external there, so prefer TEXTURE_2D + sampler2D.
        // Fall back to EXTERNAL_OES only if TEXTURE_2D target fails.
        let texture = unsafe {
            let tex = gl.create_texture().map_err(|_| "Failed to create OpenGL texture")?;

            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            target_texture(glow::TEXTURE_2D, image);
            let err_2d = gl.get_error();
            if err_2d == glow::NO_ERROR {
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
                gl.bind_texture(glow::TEXTURE_2D, None);
                return Ok(ImportedTexture {
                    texture: tex,
                    target: glow::TEXTURE_2D,
                    is_external_oes: false,
                    width: desc.width,
                    height: desc.height,
                    y_inverted: desc.y_inverted,
                    egl_image: image,
                });
            }

            // Retry as EXTERNAL_OES (needs COMP_FS_OES to sample).
            while gl.get_error() != glow::NO_ERROR {}
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, Some(tex));
            target_texture(GL_TEXTURE_EXTERNAL_OES, image);
            let err_oes = gl.get_error();
            if err_oes != glow::NO_ERROR {
                gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, None);
                gl.delete_texture(tex);
                if let Some(destroy) = self.p_destroy_image {
                    destroy(self.egl_display, image);
                }
                return Err("glEGLImageTargetTexture2DOES failed for TEXTURE_2D and EXTERNAL_OES");
            }
            gl.tex_parameter_i32(
                GL_TEXTURE_EXTERNAL_OES,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                GL_TEXTURE_EXTERNAL_OES,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                GL_TEXTURE_EXTERNAL_OES,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                GL_TEXTURE_EXTERNAL_OES,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.bind_texture(GL_TEXTURE_EXTERNAL_OES, None);
            tex
        };

        Ok(ImportedTexture {
            texture,
            target: GL_TEXTURE_EXTERNAL_OES,
            is_external_oes: true,
            width: desc.width,
            height: desc.height,
            y_inverted: desc.y_inverted,
            egl_image: image,
        })
    }

    pub fn destroy_image(&self, image: *mut c_void) {
        if !image.is_null() && !self.egl_display.is_null() {
            if let Some(destroy) = self.p_destroy_image {
                unsafe {
                    destroy(self.egl_display, image);
                }
            }
        }
    }
}

