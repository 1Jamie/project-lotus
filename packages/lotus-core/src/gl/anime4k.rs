//! Anime4K v4.x shader chain runner for Lotus Compositor.
//!
//! Enforces:
//! 1. Sub-pass splitting on //!HOOK boundaries + GL_RGBA16F intermediate textures.
//! 2. AutoDownscalePre_x4 inclusion between two 2x passes in +A secondary chains.
//! 3. Clamp_Highlights split timing: stats (passes 1 & 2) at head, clamp (pass 3) at tail.
//! 4. DMA-BUF Y-flip applied on pass 0 into MAIN; all subsequent passes use identity UVs.
//! 5. Lazy compilation per preset & reusable cached FBO pool (zero per-frame allocations).

use std::collections::HashMap;
use std::sync::Arc;
use glow::HasContext;

// Static shader sources bundled at compile time
pub mod sources {
    pub const CLAMP_HIGHLIGHTS: &str = include_str!("../../shaders/anime4k/Anime4K_Clamp_Highlights.glsl");
    pub const RESTORE_CNN_S: &str = include_str!("../../shaders/anime4k/Anime4K_Restore_CNN_S.glsl");
    pub const RESTORE_CNN_M: &str = include_str!("../../shaders/anime4k/Anime4K_Restore_CNN_M.glsl");
    pub const RESTORE_CNN_L: &str = include_str!("../../shaders/anime4k/Anime4K_Restore_CNN_L.glsl");
    pub const RESTORE_CNN_VL: &str = include_str!("../../shaders/anime4k/Anime4K_Restore_CNN_VL.glsl");
    pub const RESTORE_CNN_UL: &str = include_str!("../../shaders/anime4k/Anime4K_Restore_CNN_UL.glsl");

    pub const RESTORE_CNN_SOFT_S: &str = include_str!("../../shaders/anime4k/Anime4K_Restore_CNN_Soft_S.glsl");
    pub const RESTORE_CNN_SOFT_M: &str = include_str!("../../shaders/anime4k/Anime4K_Restore_CNN_Soft_M.glsl");
    pub const RESTORE_CNN_SOFT_L: &str = include_str!("../../shaders/anime4k/Anime4K_Restore_CNN_Soft_L.glsl");
    pub const RESTORE_CNN_SOFT_VL: &str = include_str!("../../shaders/anime4k/Anime4K_Restore_CNN_Soft_VL.glsl");
    pub const RESTORE_CNN_SOFT_UL: &str = include_str!("../../shaders/anime4k/Anime4K_Restore_CNN_Soft_UL.glsl");

    pub const UPSCALE_CNN_X2_S: &str = include_str!("../../shaders/anime4k/Anime4K_Upscale_CNN_x2_S.glsl");
    pub const UPSCALE_CNN_X2_M: &str = include_str!("../../shaders/anime4k/Anime4K_Upscale_CNN_x2_M.glsl");
    pub const UPSCALE_CNN_X2_L: &str = include_str!("../../shaders/anime4k/Anime4K_Upscale_CNN_x2_L.glsl");
    pub const UPSCALE_CNN_X2_VL: &str = include_str!("../../shaders/anime4k/Anime4K_Upscale_CNN_x2_VL.glsl");
    pub const UPSCALE_CNN_X2_UL: &str = include_str!("../../shaders/anime4k/Anime4K_Upscale_CNN_x2_UL.glsl");

    pub const UPSCALE_DENOISE_CNN_X2_S: &str = include_str!("../../shaders/anime4k/Anime4K_Upscale_Denoise_CNN_x2_S.glsl");
    pub const UPSCALE_DENOISE_CNN_X2_M: &str = include_str!("../../shaders/anime4k/Anime4K_Upscale_Denoise_CNN_x2_M.glsl");
    pub const UPSCALE_DENOISE_CNN_X2_L: &str = include_str!("../../shaders/anime4k/Anime4K_Upscale_Denoise_CNN_x2_L.glsl");
    pub const UPSCALE_DENOISE_CNN_X2_VL: &str = include_str!("../../shaders/anime4k/Anime4K_Upscale_Denoise_CNN_x2_VL.glsl");
    pub const UPSCALE_DENOISE_CNN_X2_UL: &str = include_str!("../../shaders/anime4k/Anime4K_Upscale_Denoise_CNN_x2_UL.glsl");

    pub const AUTODOWNSCALE_PRE_X2: &str = include_str!("../../shaders/anime4k/Anime4K_AutoDownscalePre_x2.glsl");
    pub const AUTODOWNSCALE_PRE_X4: &str = include_str!("../../shaders/anime4k/Anime4K_AutoDownscalePre_x4.glsl");
}

/// Evaluation dimension context for Reverse Polish Notation directives
#[derive(Debug, Clone, Copy, Default)]
pub struct DimContext {
    pub main_w: f64,
    pub main_h: f64,
    pub native_w: f64,
    pub native_h: f64,
    pub hooked_w: f64,
    pub hooked_h: f64,
    pub output_w: f64,
    pub output_h: f64,
}

impl DimContext {
    pub fn get_var(&self, name: &str, named_dims: &HashMap<String, (u32, u32)>) -> Option<f64> {
        match name {
            "MAIN.w" => Some(self.main_w),
            "MAIN.h" => Some(self.main_h),
            "NATIVE.w" => Some(self.native_w),
            "NATIVE.h" => Some(self.native_h),
            "HOOKED.w" => Some(self.hooked_w),
            "HOOKED.h" => Some(self.hooked_h),
            "OUTPUT.w" => Some(self.output_w),
            "OUTPUT.h" => Some(self.output_h),
            _ => {
                if let Some((tex_name, field)) = name.split_once('.') {
                    if let Some(&(w, h)) = named_dims.get(tex_name) {
                        return match field {
                            "w" | "width" => Some(w as f64),
                            "h" | "height" => Some(h as f64),
                            _ => None,
                        };
                    }
                }
                None
            }
        }
    }
}

/// Evaluates postfix (RPN) mathematical expressions used in //!WIDTH, //!HEIGHT, //!WHEN.
pub fn eval_rpn(expr: &str, ctx: &DimContext, named_dims: &HashMap<String, (u32, u32)>) -> Option<f64> {
    let mut stack: Vec<f64> = Vec::with_capacity(16);
    for token in expr.split_whitespace() {
        if let Ok(num) = token.parse::<f64>() {
            stack.push(num);
            continue;
        }
        if let Some(val) = ctx.get_var(token, named_dims) {
            stack.push(val);
            continue;
        }
        match token {
            "+" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(a + b);
            }
            "-" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(a - b);
            }
            "*" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(a * b);
            }
            "/" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(if b == 0.0 { 0.0 } else { a / b });
            }
            ">" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(if a > b { 1.0 } else { 0.0 });
            }
            "<" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(if a < b { 1.0 } else { 0.0 });
            }
            ">=" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(if a >= b { 1.0 } else { 0.0 });
            }
            "<=" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(if a <= b { 1.0 } else { 0.0 });
            }
            "==" | "=" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(if (a - b).abs() < 1e-6 { 1.0 } else { 0.0 });
            }
            "!=" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(if (a - b).abs() >= 1e-6 { 1.0 } else { 0.0 });
            }
            "&&" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(if a != 0.0 && b != 0.0 { 1.0 } else { 0.0 });
            }
            "||" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(if a != 0.0 || b != 0.0 { 1.0 } else { 0.0 });
            }
            "!" => {
                let a = stack.pop()?;
                stack.push(if a == 0.0 { 1.0 } else { 0.0 });
            }
            _ => return None,
        }
    }
    stack.pop()
}

/// Parsed sub-pass directives and body
#[derive(Debug, Clone)]
pub struct SubPassDesc {
    pub desc: String,
    pub hook_target: String,
    pub binds: Vec<String>,
    pub save_target: Option<String>,
    pub width_rpn: Option<String>,
    pub height_rpn: Option<String>,
    pub when_rpn: Option<String>,
    pub components: u32,
    pub body: String,
    /// When true, binds should be scoped to a pass-unique prefix (for intermediate conv2d)
    pub slot_index: usize,
}

impl SubPassDesc {
    pub fn generate_fragment_shader(&self, scoped_binds: &HashMap<String, String>) -> String {
        let mut fs = String::with_capacity(self.body.len() + 1024);
        fs.push_str("#version 330 core\n");
        fs.push_str("in vec2 v_uv;\n");
        fs.push_str("#define v_texcoord v_uv\n");
        fs.push_str("out vec4 FragColor;\n\n");

        for bind_orig in &self.binds {
            // Use the scoped name in uniforms and macros
            let bind_name = scoped_binds.get(bind_orig).map(|s| s.as_str()).unwrap_or(bind_orig.as_str());
            fs.push_str(&format!("uniform sampler2D {}_raw;\n", bind_name));
            fs.push_str(&format!("uniform vec2 {}_size;\n", bind_name));
            fs.push_str(&format!("uniform vec2 {}_pt;\n", bind_name));
            fs.push_str(&format!("#define {}_pos v_texcoord\n", bind_name));
            fs.push_str(&format!("#define {}_mul 1.0\n", bind_name));
            fs.push_str(&format!("#define {}_off vec2(0.0)\n", bind_name));
            fs.push_str(&format!("#define {}_tex(pos) textureLod({}_raw, (pos), 0.0)\n", bind_name, bind_name));
            fs.push_str(&format!("#define {}_texOff(off) textureLod({}_raw, v_texcoord + {}_pt * vec2(off), 0.0)\n\n", bind_name, bind_name, bind_name));

            // Also alias original bind name to scoped name if different
            if bind_name != bind_orig {
                fs.push_str(&format!("#define {}_raw {}_raw\n", bind_orig, bind_name));
                fs.push_str(&format!("#define {}_size {}_size\n", bind_orig, bind_name));
                fs.push_str(&format!("#define {}_pt {}_pt\n", bind_orig, bind_name));
                fs.push_str(&format!("#define {}_pos {}_pos\n", bind_orig, bind_name));
                fs.push_str(&format!("#define {}_mul {}_mul\n", bind_orig, bind_name));
                fs.push_str(&format!("#define {}_off {}_off\n", bind_orig, bind_name));
                fs.push_str(&format!("#define {}_tex(pos) {}_tex(pos)\n", bind_orig, bind_name));
                fs.push_str(&format!("#define {}_texOff(off) {}_texOff(off)\n\n", bind_orig, bind_name));
            }
        }

        // In mpv/libplacebo, //!HOOK MAIN shaders may reference MAIN_tex/MAIN_texOff
        // even when only //!BIND HOOKED is declared (e.g., Clamp_Highlights).
        if self.hook_target == "MAIN"
            && self.binds.iter().any(|b| b == "HOOKED")
            && !self.binds.iter().any(|b| b == "MAIN")
        {
            fs.push_str("#define MAIN_raw HOOKED_raw\n");
            fs.push_str("#define MAIN_size HOOKED_size\n");
            fs.push_str("#define MAIN_pt HOOKED_pt\n");
            fs.push_str("#define MAIN_pos HOOKED_pos\n");
            fs.push_str("#define MAIN_mul HOOKED_mul\n");
            fs.push_str("#define MAIN_off HOOKED_off\n");
            fs.push_str("#define MAIN_tex(pos) HOOKED_tex(pos)\n");
            fs.push_str("#define MAIN_texOff(off) HOOKED_texOff(off)\n\n");
        }

        fs.push_str(&self.body);
        fs.push_str("\n\nvoid main() {\n    FragColor = hook();\n}\n");
        fs
    }
}

/// Parse a raw .glsl file into a sequence of SubPassDesc objects
pub fn parse_glsl_file(content: &str, slot_index: usize) -> Vec<SubPassDesc> {
    let mut passes = Vec::new();
    let lines = content.lines();

    let mut current_desc = String::new();
    let mut current_hook = String::new();
    let mut current_binds = Vec::new();
    let mut current_save = None;
    let mut current_width = None;
    let mut current_height = None;
    let mut current_when = None;
    let mut current_components = 4;
    let mut current_body = Vec::new();
    let mut in_pass = false;

    let finish_pass = |passes: &mut Vec<SubPassDesc>,
                       desc: &str,
                       hook: &str,
                       binds: &[String],
                       save: Option<String>,
                       w_rpn: Option<String>,
                       h_rpn: Option<String>,
                       when_rpn: Option<String>,
                       comps: u32,
                       body_lines: &[&str],
                       slot: usize| {
        if !hook.is_empty() {
            passes.push(SubPassDesc {
                desc: desc.to_string(),
                hook_target: hook.to_string(),
                binds: binds.to_vec(),
                save_target: save,
                width_rpn: w_rpn,
                height_rpn: h_rpn,
                when_rpn,
                components: comps,
                body: body_lines.join("\n"),
                slot_index: slot,
            });
        }
    };

    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("//!HOOK") {
            if in_pass {
                finish_pass(
                    &mut passes,
                    &current_desc,
                    &current_hook,
                    &current_binds,
                    current_save.take(),
                    current_width.take(),
                    current_height.take(),
                    current_when.take(),
                    current_components,
                    &current_body,
                    slot_index,
                );
                current_desc.clear();
                current_binds.clear();
                current_body.clear();
                current_components = 4;
            }
            in_pass = true;
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            current_hook = parts.get(1).unwrap_or(&"MAIN").to_string();
            continue;
        }

        if trimmed.starts_with("//!DESC") {
            current_desc = trimmed.strip_prefix("//!DESC").unwrap_or("").trim().to_string();
            continue;
        }
        if trimmed.starts_with("//!BIND") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if let Some(target) = parts.get(1) {
                if !current_binds.contains(&target.to_string()) {
                    current_binds.push(target.to_string());
                }
            }
            continue;
        }
        if trimmed.starts_with("//!SAVE") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if let Some(target) = parts.get(1) {
                current_save = Some(target.to_string());
            }
            continue;
        }
        if trimmed.starts_with("//!WIDTH") {
            current_width = Some(trimmed.strip_prefix("//!WIDTH").unwrap_or("").trim().to_string());
            continue;
        }
        if trimmed.starts_with("//!HEIGHT") {
            current_height = Some(trimmed.strip_prefix("//!HEIGHT").unwrap_or("").trim().to_string());
            continue;
        }
        if trimmed.starts_with("//!WHEN") {
            current_when = Some(trimmed.strip_prefix("//!WHEN").unwrap_or("").trim().to_string());
            continue;
        }
        if trimmed.starts_with("//!COMPONENTS") {
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if let Some(c) = parts.get(1).and_then(|s| s.parse().ok()) {
                current_components = c;
            }
            continue;
        }

        if in_pass {
            current_body.push(line);
        }
    }

    if in_pass && !current_hook.is_empty() {
        finish_pass(
            &mut passes,
            &current_desc,
            &current_hook,
            &current_binds,
            current_save,
            current_width,
            current_height,
            current_when,
            current_components,
            &current_body,
            slot_index,
        );
    }

    passes
}

/// Canonical preset configuration
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Anime4kPresetKey {
    pub mode: String,     // "a", "b", "c"
    pub tier: String,     // "s", "m", "l", "vl", "ul"
    pub plus_a: bool,
}

/// Uniform locations for a bound texture (each optional since GLSL may optimize out unused uniforms)
pub struct BindUniforms {
    pub raw: Option<glow::UniformLocation>,
    pub size: Option<glow::UniformLocation>,
    pub pt: Option<glow::UniformLocation>,
}

/// Compiled sub-pass with cached uniform locations
pub struct CompiledSubPass {
    pub desc: SubPassDesc,
    pub program: glow::Program,
    pub bind_locs: HashMap<String, BindUniforms>,
}

/// Persistent texture wrapper
pub struct FboTexture {
    pub tex: glow::NativeTexture,
    pub fbo: glow::NativeFramebuffer,
    pub width: u32,
    pub height: u32,
    pub is_fp16: bool,
}

impl FboTexture {
    pub fn new(gl: &glow::Context, width: u32, height: u32, is_fp16: bool) -> Result<Self, String> {
        unsafe {
            let tex = gl.create_texture().map_err(|e| e.to_string())?;
            let fbo = gl.create_framebuffer().map_err(|e| e.to_string())?;

            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);

            let (internal_fmt, data_type) = if is_fp16 {
                (glow::RGBA16F, glow::HALF_FLOAT)
            } else {
                (glow::RGBA8, glow::UNSIGNED_BYTE)
            };

            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal_fmt as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                data_type,
                glow::PixelUnpackData::Slice(None),
            );

            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );

            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if status != glow::FRAMEBUFFER_COMPLETE {
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                gl.bind_texture(glow::TEXTURE_2D, None);
                return Err(format!("FBO creation incomplete: status 0x{:x}", status));
            }

            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.bind_texture(glow::TEXTURE_2D, None);

            Ok(Self {
                tex,
                fbo,
                width,
                height,
                is_fp16,
            })
        }
    }

    pub fn resize(&mut self, gl: &glow::Context, width: u32, height: u32) {
        if self.width == width && self.height == height {
            return;
        }
        self.width = width.max(1);
        self.height = height.max(1);
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.tex));
            let (internal_fmt, data_type) = if self.is_fp16 {
                (glow::RGBA16F, glow::HALF_FLOAT)
            } else {
                (glow::RGBA8, glow::UNSIGNED_BYTE)
            };
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                internal_fmt as i32,
                self.width as i32,
                self.height as i32,
                0,
                glow::RGBA,
                data_type,
                glow::PixelUnpackData::Slice(None),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    pub fn destroy(&self, gl: &glow::Context) {
        unsafe {
            gl.delete_framebuffer(self.fbo);
            gl.delete_texture(self.tex);
        }
    }
}

/// Anime4K state container holding compiled shaders and pooled FBO textures.
pub struct Anime4kPipeline {
    pub gl: Arc<glow::Context>,
    pub comp_vs: glow::NativeShader,
    pub comp_vao: glow::NativeVertexArray,
    /// Cached compiled pipelines by preset
    pub compiled_presets: HashMap<Anime4kPresetKey, Vec<CompiledSubPass>>,
    /// Named persistent FBO textures (STATSMAX, conv2d intermediates, etc.)
    pub fbo_pool: HashMap<String, FboTexture>,
    /// Primary ping-pong FBOs for MAIN
    pub main_ping: Option<FboTexture>,
    pub main_pong: Option<FboTexture>,
    /// Last logged diagnostic signature (preset, src_w, src_h, dst_w, dst_h)
    pub last_diag_sig: Option<(Anime4kPresetKey, u32, u32, u32, u32)>,
}

impl Anime4kPipeline {
    pub fn new(gl: Arc<glow::Context>, comp_vs: glow::NativeShader, comp_vao: glow::NativeVertexArray) -> Self {
        Self {
            gl,
            comp_vs,
            comp_vao,
            compiled_presets: HashMap::new(),
            fbo_pool: HashMap::new(),
            main_ping: None,
            main_pong: None,
            last_diag_sig: None,
        }
    }

    /// Build the canonical chain list based on Mode, Tier, and +A
    pub fn build_canonical_chain(&self, key: &Anime4kPresetKey) -> Vec<SubPassDesc> {
        build_canonical_preset_chain(key)
    }
}

pub fn build_canonical_preset_chain(key: &Anime4kPresetKey) -> Vec<SubPassDesc> {
    let mode = key.mode.as_str();
    let tier = key.tier.as_str();
    let plus_a = key.plus_a;

        // Step 1: Parse Clamp Highlights
        let clamp_passes = parse_glsl_file(sources::CLAMP_HIGHLIGHTS, 0);
        let clamp_stats: Vec<_> = clamp_passes.iter().filter(|p| p.hook_target == "MAIN").cloned().collect();
        let clamp_apply = clamp_passes.iter().find(|p| p.hook_target == "PREKERNEL").cloned();

        let mut chain = Vec::new();
        // 1. Clamp_Highlights stats sub-passes run first
        chain.extend(clamp_stats);

        let mut slot_counter = 1;

        let get_restore_source = |t: &str, soft: bool| -> &'static str {
            match (soft, t) {
                (false, "s") => sources::RESTORE_CNN_S,
                (false, "m") => sources::RESTORE_CNN_M,
                (false, "l") => sources::RESTORE_CNN_L,
                (false, "vl") => sources::RESTORE_CNN_VL,
                (false, "ul") => sources::RESTORE_CNN_UL,
                (true, "s") => sources::RESTORE_CNN_SOFT_S,
                (true, "m") => sources::RESTORE_CNN_SOFT_M,
                (true, "l") => sources::RESTORE_CNN_SOFT_L,
                (true, "vl") => sources::RESTORE_CNN_SOFT_VL,
                (true, "ul") => sources::RESTORE_CNN_SOFT_UL,
                _ => sources::RESTORE_CNN_M,
            }
        };

        let get_upscale_source = |t: &str| -> &'static str {
            match t {
                "s" => sources::UPSCALE_CNN_X2_S,
                "m" => sources::UPSCALE_CNN_X2_M,
                "l" => sources::UPSCALE_CNN_X2_L,
                "vl" => sources::UPSCALE_CNN_X2_VL,
                "ul" => sources::UPSCALE_CNN_X2_UL,
                _ => sources::UPSCALE_CNN_X2_M,
            }
        };

        let get_upscale_denoise_source = |t: &str| -> &'static str {
            match t {
                "s" => sources::UPSCALE_DENOISE_CNN_X2_S,
                "m" => sources::UPSCALE_DENOISE_CNN_X2_M,
                "l" => sources::UPSCALE_DENOISE_CNN_X2_L,
                "vl" => sources::UPSCALE_DENOISE_CNN_X2_VL,
                "ul" => sources::UPSCALE_DENOISE_CNN_X2_UL,
                _ => sources::UPSCALE_DENOISE_CNN_X2_M,
            }
        };

        match mode {
            "a" => {
                // Restore -> Upscale -> (AutoDownscalePre_x4 + second Restore -> Upscale if +A) -> AutoDownscalePre_x2 -> Restore_CNN_S
                chain.extend(parse_glsl_file(get_restore_source(tier, false), slot_counter));
                slot_counter += 1;
                chain.extend(parse_glsl_file(get_upscale_source(tier), slot_counter));
                slot_counter += 1;

                if plus_a {
                    // INVARIANT 2: AutoDownscalePre_x4 inserted between the two 2x passes
                    chain.extend(parse_glsl_file(sources::AUTODOWNSCALE_PRE_X4, slot_counter));
                    slot_counter += 1;
                    chain.extend(parse_glsl_file(get_restore_source(tier, false), slot_counter));
                    slot_counter += 1;
                    chain.extend(parse_glsl_file(get_upscale_source(tier), slot_counter));
                    slot_counter += 1;
                }

                chain.extend(parse_glsl_file(sources::AUTODOWNSCALE_PRE_X2, slot_counter));
                slot_counter += 1;
                chain.extend(parse_glsl_file(sources::RESTORE_CNN_S, slot_counter));
            }
            "b" => {
                // Restore_Soft -> Upscale -> (+A variant) -> AutoDownscalePre_x2 -> Restore_CNN_Soft_S
                chain.extend(parse_glsl_file(get_restore_source(tier, true), slot_counter));
                slot_counter += 1;
                chain.extend(parse_glsl_file(get_upscale_source(tier), slot_counter));
                slot_counter += 1;

                if plus_a {
                    chain.extend(parse_glsl_file(sources::AUTODOWNSCALE_PRE_X4, slot_counter));
                    slot_counter += 1;
                    chain.extend(parse_glsl_file(get_restore_source(tier, true), slot_counter));
                    slot_counter += 1;
                    chain.extend(parse_glsl_file(get_upscale_source(tier), slot_counter));
                    slot_counter += 1;
                }

                chain.extend(parse_glsl_file(sources::AUTODOWNSCALE_PRE_X2, slot_counter));
                slot_counter += 1;
                chain.extend(parse_glsl_file(sources::RESTORE_CNN_SOFT_S, slot_counter));
            }
            "c" | _ => {
                // Upscale_Denoise -> (+A variant) -> AutoDownscalePre_x2 -> Restore_CNN_S
                chain.extend(parse_glsl_file(get_upscale_denoise_source(tier), slot_counter));
                slot_counter += 1;

                if plus_a {
                    chain.extend(parse_glsl_file(sources::AUTODOWNSCALE_PRE_X4, slot_counter));
                    slot_counter += 1;
                    chain.extend(parse_glsl_file(get_restore_source(tier, false), slot_counter));
                    slot_counter += 1;
                    chain.extend(parse_glsl_file(get_upscale_source(tier), slot_counter));
                    slot_counter += 1;
                }

                chain.extend(parse_glsl_file(sources::AUTODOWNSCALE_PRE_X2, slot_counter));
                slot_counter += 1;
                chain.extend(parse_glsl_file(sources::RESTORE_CNN_S, slot_counter));
            }
        }

        // 3. Clamp_Highlights clamp sub-pass runs last
        if let Some(apply) = clamp_apply {
            chain.push(apply);
        }

        chain
    }

impl Anime4kPipeline {
    /// Compile a pipeline for a specific preset key lazily
    pub fn ensure_compiled_preset(&mut self, key: &Anime4kPresetKey) -> Result<(), String> {
        if self.compiled_presets.contains_key(key) {
            return Ok(());
        }

        let chain_desc = self.build_canonical_chain(key);
        let mut compiled = Vec::with_capacity(chain_desc.len());

        for pass in chain_desc {
            // Build scoped bind mapping
            let mut scoped_binds = HashMap::new();
            for b in &pass.binds {
                // STATSMAX, MAIN, HOOKED are global; conv2d tensors are scoped to the file slot
                if b.starts_with("conv2d") {
                    scoped_binds.insert(b.clone(), format!("slot{}_{}", pass.slot_index, b));
                } else {
                    scoped_binds.insert(b.clone(), b.clone());
                }
            }

            let fs_source = pass.generate_fragment_shader(&scoped_binds);
            unsafe {
                let program = self.gl.create_program().map_err(|e| e.to_string())?;
                let fs = self.gl.create_shader(glow::FRAGMENT_SHADER).map_err(|e| e.to_string())?;

                self.gl.shader_source(fs, &fs_source);
                self.gl.compile_shader(fs);
                if !self.gl.get_shader_compile_status(fs) {
                    let log = self.gl.get_shader_info_log(fs);
                    self.gl.delete_shader(fs);
                    self.gl.delete_program(program);
                    return Err(format!("Shader compile error in pass '{}': {}", pass.desc, log));
                }

                self.gl.attach_shader(program, self.comp_vs);
                self.gl.attach_shader(program, fs);
                self.gl.link_program(program);
                self.gl.detach_shader(program, self.comp_vs);
                self.gl.detach_shader(program, fs);
                self.gl.delete_shader(fs);

                if !self.gl.get_program_link_status(program) {
                    let log = self.gl.get_program_info_log(program);
                    self.gl.delete_program(program);
                    return Err(format!("Shader link error in pass '{}': {}", pass.desc, log));
                }

                // Cache uniform locations for all bound textures
                let mut bind_locs = HashMap::new();
                for b in &pass.binds {
                    let bind_name = scoped_binds.get(b).unwrap_or(b);
                    let raw = self.gl.get_uniform_location(program, &format!("{}_raw", bind_name));
                    let size = self.gl.get_uniform_location(program, &format!("{}_size", bind_name));
                    let pt = self.gl.get_uniform_location(program, &format!("{}_pt", bind_name));
                    if raw.is_some() || size.is_some() || pt.is_some() {
                        bind_locs.insert(b.clone(), BindUniforms { raw, size, pt });
                    }
                }

                compiled.push(CompiledSubPass {
                    desc: pass,
                    program,
                    bind_locs,
                });
            }
        }

        log::info!(
            "[lotus-anime4k] compiled preset key: mode={} tier={} +A={} ({} sub-passes)",
            key.mode,
            key.tier,
            key.plus_a,
            compiled.len()
        );
        self.compiled_presets.insert(key.clone(), compiled);
        Ok(())
    }

    /// Execute the complete Anime4K pipeline.
    /// Returns the NativeTexture containing the final upscaled result.
    pub fn execute(
        &mut self,
        source_tex: glow::NativeTexture,
        source_w: u32,
        source_h: u32,
        dest_w: u32,
        dest_h: u32,
        key: &Anime4kPresetKey,
    ) -> Result<glow::NativeTexture, String> {
        self.ensure_compiled_preset(key)?;
        let diag_sig = (key.clone(), source_w, source_h, dest_w, dest_h);
        let log_diag = self.last_diag_sig.as_ref() != Some(&diag_sig);
        if log_diag {
            self.last_diag_sig = Some(diag_sig);
            log::info!(
                "[lotus-anime4k] diag run mode={} tier={} +A={} src={}x{} dst={}x{}",
                key.mode, key.tier, key.plus_a, source_w, source_h, dest_w, dest_h
            );
        }
        let passes = self.compiled_presets.get(key).unwrap();

        let gl = &self.gl;

        // Ensure MAIN ping-pong FBOs exist with GL_RGBA16F
        if self.main_ping.is_none() {
            self.main_ping = Some(FboTexture::new(gl, source_w, source_h, true)?);
        }
        if self.main_pong.is_none() {
            self.main_pong = Some(FboTexture::new(gl, source_w, source_h, true)?);
        }

        // Texture dimension map
        let mut named_dims: HashMap<String, (u32, u32)> = HashMap::new();
        named_dims.insert("MAIN".to_string(), (source_w, source_h));
        named_dims.insert("NATIVE".to_string(), (source_w, source_h));

        // Texture storage map (scoped_name -> NativeTexture)
        let mut named_textures: HashMap<String, glow::NativeTexture> = HashMap::new();
        named_textures.insert("MAIN".to_string(), source_tex);

        let mut current_hooked_tex = source_tex;
        let mut current_hooked_w = source_w;
        let mut current_hooked_h = source_h;

        let mut ping_is_source = true;

        let mut ctx = DimContext {
            main_w: source_w as f64,
            main_h: source_h as f64,
            native_w: source_w as f64,
            native_h: source_h as f64,
            hooked_w: current_hooked_w as f64,
            hooked_h: current_hooked_h as f64,
            output_w: dest_w as f64,
            output_h: dest_h as f64,
        };

        for (pass_idx, pass) in passes.iter().enumerate() {
            ctx.hooked_w = current_hooked_w as f64;
            ctx.hooked_h = current_hooked_h as f64;

            // Evaluate //!WHEN condition if present
            if let Some(ref when_expr) = pass.desc.when_rpn {
                if let Some(res) = eval_rpn(when_expr, &ctx, &named_dims) {
                    if res <= 0.5 {
                        if log_diag {
                            log::info!(
                                "[lotus-anime4k]   pass[{}] SKIP '{}' (when='{}' -> {})",
                                pass_idx, pass.desc.desc, when_expr, res
                            );
                        }
                        // Pass condition failed; skip execution
                        continue;
                    }
                }
            }

            // Determine target pass width and height
            let pass_w = if let Some(ref w_expr) = pass.desc.width_rpn {
                eval_rpn(w_expr, &ctx, &named_dims).map(|v| v as u32).unwrap_or(current_hooked_w)
            } else {
                current_hooked_w
            }.max(1);

            let pass_h = if let Some(ref h_expr) = pass.desc.height_rpn {
                eval_rpn(h_expr, &ctx, &named_dims).map(|v| v as u32).unwrap_or(current_hooked_h)
            } else {
                current_hooked_h
            }.max(1);

            // Determine output target FBO: either a named intermediate (//!SAVE <NAME>) or MAIN
            let (target_fbo, target_tex, target_scoped_name) = if let Some(ref save_name) = pass.desc.save_target {
                if save_name == "MAIN" {
                    // Writes back to MAIN ping-pong
                    let ping = self.main_ping.as_mut().unwrap();
                    let pong = self.main_pong.as_mut().unwrap();
                    let (dst_fbo, dst_tex) = if ping_is_source {
                        pong.resize(gl, pass_w, pass_h);
                        (pong.fbo, pong.tex)
                    } else {
                        ping.resize(gl, pass_w, pass_h);
                        (ping.fbo, ping.tex)
                    };
                    ping_is_source = !ping_is_source;
                    (dst_fbo, dst_tex, "MAIN".to_string())
                } else {
                    // Named persistent intermediate texture (e.g. STATSMAX or slot-scoped conv2d)
                    let scoped_save = if save_name == "STATSMAX" {
                        save_name.clone()
                    } else {
                        format!("slot{}_{}", pass.desc.slot_index, save_name)
                    };

                    let entry = self.fbo_pool.entry(scoped_save.clone()).or_insert_with(|| {
                        FboTexture::new(gl, pass_w, pass_h, true).expect("Failed to allocate intermediate FBO")
                    });
                    entry.resize(gl, pass_w, pass_h);
                    (entry.fbo, entry.tex, scoped_save)
                }
            } else {
                // No //!SAVE directive implies output to MAIN
                let ping = self.main_ping.as_mut().unwrap();
                let pong = self.main_pong.as_mut().unwrap();
                let (dst_fbo, dst_tex) = if ping_is_source {
                    pong.resize(gl, pass_w, pass_h);
                    (pong.fbo, pong.tex)
                } else {
                    ping.resize(gl, pass_w, pass_h);
                    (ping.fbo, ping.tex)
                };
                ping_is_source = !ping_is_source;
                (dst_fbo, dst_tex, "MAIN".to_string())
            };

            // Render pass
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target_fbo));
                gl.viewport(0, 0, pass_w as i32, pass_h as i32);
                gl.disable(glow::BLEND);
                gl.disable(glow::SCISSOR_TEST);

                gl.use_program(Some(pass.program));
                gl.bind_vertex_array(Some(self.comp_vao));

                // Bind all textures declared in //!BIND
                let mut texture_unit = 0;
                for bind_name in &pass.desc.binds {
                    let scoped_bind = if bind_name.starts_with("conv2d") {
                        format!("slot{}_{}", pass.desc.slot_index, bind_name)
                    } else {
                        bind_name.clone()
                    };

                    let (bind_tex, (bw, bh)) = if bind_name == "HOOKED" {
                        (current_hooked_tex, (current_hooked_w, current_hooked_h))
                    } else if bind_name == "MAIN" {
                        let m_tex = named_textures.get("MAIN").copied().unwrap_or(source_tex);
                        let &(mw, mh) = named_dims.get("MAIN").unwrap_or(&(source_w, source_h));
                        (m_tex, (mw, mh))
                    } else if let Some(&tex) = named_textures.get(&scoped_bind) {
                        let &(w, h) = named_dims.get(&scoped_bind).unwrap_or(&(pass_w, pass_h));
                        (tex, (w, h))
                    } else {
                        (source_tex, (source_w, source_h))
                    };

                    if let Some(locs) = pass.bind_locs.get(bind_name) {
                        if let Some(ref raw_l) = locs.raw {
                            let filter = if bind_name.starts_with("conv2d") {
                                glow::NEAREST
                            } else {
                                glow::LINEAR
                            };
                            gl.active_texture(glow::TEXTURE0 + texture_unit);
                            gl.bind_texture(glow::TEXTURE_2D, Some(bind_tex));
                            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, filter as i32);
                            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, filter as i32);
                            gl.uniform_1_i32(Some(raw_l), texture_unit as i32);
                            texture_unit += 1;
                        }
                        if let Some(ref size_l) = locs.size {
                            gl.uniform_2_f32(Some(size_l), bw as f32, bh as f32);
                        }
                        if let Some(ref pt_l) = locs.pt {
                            gl.uniform_2_f32(Some(pt_l), 1.0 / bw as f32, 1.0 / bh as f32);
                        }
                    }
                }

                // INVARIANT 4: If pass 0 sampling game surface with flip_y, vertices or UVs match.
                // Our comp_vao draws full-screen quad [-1,1].
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                if log_diag {
                    let err = gl.get_error();
                    log::info!(
                        "[lotus-anime4k]   pass[{}] RUN '{}' -> {} ({}x{}, units={}, gl_err=0x{:x})",
                        pass_idx, pass.desc.desc, target_scoped_name, pass_w, pass_h, texture_unit, err
                    );
                }

                // Unbind textures
                for u in 0..texture_unit {
                    gl.active_texture(glow::TEXTURE0 + u);
                    gl.bind_texture(glow::TEXTURE_2D, None);
                }
                gl.bind_vertex_array(None);
                gl.use_program(None);
            }

            // Record updated texture in maps
            named_dims.insert(target_scoped_name.clone(), (pass_w, pass_h));
            if let Some(ref save_name) = pass.desc.save_target {
                named_dims.insert(save_name.clone(), (pass_w, pass_h));
            }
            named_textures.insert(target_scoped_name.clone(), target_tex);

            if target_scoped_name == "MAIN" {
                current_hooked_tex = target_tex;
                current_hooked_w = pass_w;
                current_hooked_h = pass_h;
                named_dims.insert("MAIN".to_string(), (pass_w, pass_h));
                named_textures.insert("MAIN".to_string(), target_tex);
                ctx.main_w = pass_w as f64;
                ctx.main_h = pass_h as f64;
            }
        }

        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }

        Ok(current_hooked_tex)
    }

    pub fn destroy(&mut self) {
        let gl = &self.gl;
        if let Some(ref ping) = self.main_ping {
            ping.destroy(gl);
        }
        if let Some(ref pong) = self.main_pong {
            pong.destroy(gl);
        }
        self.main_ping = None;
        self.main_pong = None;

        for (_, fbo) in self.fbo_pool.drain() {
            fbo.destroy(gl);
        }

        unsafe {
            for (_, passes) in self.compiled_presets.drain() {
                for p in passes {
                    gl.delete_program(p.program);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rpn_evaluator() {
        let mut ctx = DimContext {
            main_w: 1280.0,
            main_h: 720.0,
            native_w: 1280.0,
            native_h: 720.0,
            output_w: 1920.0,
            output_h: 1080.0,
            hooked_w: 1280.0,
            hooked_h: 720.0,
        };
        let named_dims = HashMap::new();

        // Width/Height multiplication
        let res = eval_rpn("MAIN.w 2 *", &ctx, &named_dims);
        assert_eq!(res, Some(2560.0));

        // Conditional > 1.200
        let res = eval_rpn("OUTPUT.w MAIN.w / 1.200 >", &ctx, &named_dims);
        assert_eq!(res, Some(1.0)); // 1920 / 1280 = 1.5 > 1.2

        // Condition when output matches main (1.0x scale)
        ctx.output_w = 1280.0;
        ctx.output_h = 720.0;
        let res = eval_rpn("OUTPUT.w MAIN.w / 1.200 >", &ctx, &named_dims);
        assert_eq!(res, Some(0.0));

        // AutoDownscalePre_x2 condition at 1.5x scale (1920x1080 from 1280x720)
        ctx.output_w = 1920.0;
        ctx.output_h = 1080.0;
        let x2_expr = "OUTPUT.w NATIVE.w / 2.0 < OUTPUT.h NATIVE.h / 2.0 < * OUTPUT.w NATIVE.w / 1.2 > OUTPUT.h NATIVE.h / 1.2 > * *";
        let res = eval_rpn(x2_expr, &ctx, &named_dims);
        assert_eq!(res, Some(1.0));

        // AutoDownscalePre_x4 condition at 1.5x scale -> false (since 1.5 < 2.4)
        let x4_expr = "OUTPUT.w NATIVE.w / 4.0 < OUTPUT.h NATIVE.h / 4.0 < * OUTPUT.w NATIVE.w / 2.4 > OUTPUT.h NATIVE.h / 2.4 > * *";
        let res = eval_rpn(x4_expr, &ctx, &named_dims);
        assert_eq!(res, Some(0.0));

        // AutoDownscalePre_x4 condition at 3.0x scale -> true
        ctx.output_w = 3840.0;
        ctx.output_h = 2160.0;
        let res = eval_rpn(x4_expr, &ctx, &named_dims);
        assert_eq!(res, Some(1.0));
    }

    #[test]
    fn test_parse_hook_file_clamp_highlights() {
        let passes = parse_glsl_file(sources::CLAMP_HIGHLIGHTS, 0);
        // Upstream Clamp_Highlights has 3 sub-passes:
        // Pass 0: Kernel X (//!HOOK MAIN, //!SAVE STATSMAX)
        // Pass 1: Kernel Y (//!HOOK MAIN, //!SAVE STATSMAX)
        // Pass 2: Clamp (//!HOOK PREKERNEL, //!BIND STATSMAX)
        assert_eq!(passes.len(), 3);
        assert_eq!(passes[0].hook_target, "MAIN");
        assert_eq!(passes[0].save_target.as_deref(), Some("STATSMAX"));
        assert_eq!(passes[1].hook_target, "MAIN");
        assert_eq!(passes[1].save_target.as_deref(), Some("STATSMAX"));
        assert_eq!(passes[2].hook_target, "PREKERNEL");
        assert_eq!(passes[2].save_target, None);
        assert!(passes[2].binds.contains(&"STATSMAX".to_string()));
    }

    #[test]
    fn test_parse_hook_file_restore_cnn() {
        let passes = parse_glsl_file(sources::RESTORE_CNN_M, 1);
        // Multi-stage conv2d passes
        assert!(passes.len() > 1);
        for p in &passes {
            assert_eq!(p.slot_index, 1);
            assert_eq!(p.components, 4);
        }
    }

    #[test]
    fn test_preset_chain_structure() {
        // Mode A
        let key_a = Anime4kPresetKey {
            mode: "a".to_string(),
            tier: "m".to_string(),
            plus_a: false,
        };
        let chain_a = build_canonical_preset_chain(&key_a);
        // Starts with Clamp highlights 1 & 2
        assert_eq!(chain_a[0].slot_index, 0);
        assert_eq!(chain_a[0].save_target.as_deref(), Some("STATSMAX"));
        assert_eq!(chain_a[1].slot_index, 0);
        assert_eq!(chain_a[1].save_target.as_deref(), Some("STATSMAX"));
        // Ends with Clamp highlights 3
        let last = chain_a.last().unwrap();
        assert_eq!(last.hook_target, "PREKERNEL");
        assert_eq!(last.slot_index, 0);

        // Mode A + A contains AutoDownscalePre_x4
        let key_a_plus = Anime4kPresetKey {
            mode: "a".to_string(),
            tier: "m".to_string(),
            plus_a: true,
        };
        let chain_a_plus = build_canonical_preset_chain(&key_a_plus);
        let has_x4 = chain_a_plus.iter().any(|p| {
            if let Some(ref w) = p.when_rpn {
                w.contains("/ 2.4 >")
            } else {
                false
            }
        });
        assert!(has_x4, "Mode A+A must include AutoDownscalePre_x4");
    }
}
