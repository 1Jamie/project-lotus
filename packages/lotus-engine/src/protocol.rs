//! lotus-engine Remote Input & Control Protocol
//!
//! Serialization contract between Host Process (Node / lotus-core) and Secure Engine (lotus-engine).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlMessage {
    CreateSurface {
        surface_id: String,
        width: u32,
        height: u32,
        #[serde(default)]
        max_width: Option<u32>,
        #[serde(default)]
        max_height: Option<u32>,
        #[serde(default)]
        scale_factor: Option<f32>,
        url: Option<String>,
        #[serde(default)]
        bridge_script: Option<String>,
    },
    DestroySurface {
        surface_id: String,
    },
    Navigate {
        surface_id: String,
        navigation_id: u64,
        url: String,
    },
    /// Inject / re-run a script in the surface's active document (per-document
    /// secure IPC bridge after navigation credential rotation).
    EvaluateScript {
        surface_id: String,
        script: String,
    },
    Resize {
        surface_id: String,
        generation: u64,
        width: u32,
        height: u32,
        scale_factor: f32,
    },
    /// Replace the sealed pixel memfd (SCM_RIGHTS: one RW fd). Ack plane unchanged.
    /// Engine remaps, sets prod_resize_gen, resizes the webview; host keeps the old
    /// consumer map until pub_seq >= 1 on the new plane.
    ReplacePixelPlane {
        surface_id: String,
        generation: u64,
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
        scale_factor: f32,
    },
    Destroy {
        surface_id: String,
    },
    Input {
        event: InputEvent,
    },
    /// Authoritative display refresh rate from the host monitor (millihertz).
    SetDisplayTiming {
        refresh_rate_millihertz: u32,
    },
    /// One host compositor frame tick for a surface (drives jail RefreshDriver).
    Vsync {
        surface_id: String,
        presentation_time_ns: u64,
    },
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum InputEvent {
    MouseMove {
        surface_id: String,
        generation: u64,
        x: f64,
        y: f64,
        modifiers: u32,
    },
    MouseDown {
        surface_id: String,
        generation: u64,
        button: u32,
        x: f64,
        y: f64,
        modifiers: u32,
    },
    MouseUp {
        surface_id: String,
        generation: u64,
        button: u32,
        x: f64,
        y: f64,
        modifiers: u32,
    },
    MouseWheel {
        surface_id: String,
        generation: u64,
        #[serde(default)]
        x: f64,
        #[serde(default)]
        y: f64,
        dx: f64,
        dy: f64,
        modifiers: u32,
    },
    KeyDown {
        surface_id: String,
        generation: u64,
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        keycode: u32,
        #[serde(default)]
        modifiers: u32,
        #[serde(default)]
        repeat: bool,
    },
    KeyUp {
        surface_id: String,
        generation: u64,
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        keycode: u32,
        #[serde(default)]
        modifiers: u32,
    },
    Focus {
        surface_id: String,
        generation: u64,
        focused: bool,
    },
    CursorEnter {
        surface_id: String,
        generation: u64,
    },
    CursorLeave {
        surface_id: String,
        generation: u64,
    },
}

impl InputEvent {
    pub fn surface_id(&self) -> &str {
        match self {
            InputEvent::MouseMove { surface_id, .. } => surface_id,
            InputEvent::MouseDown { surface_id, .. } => surface_id,
            InputEvent::MouseUp { surface_id, .. } => surface_id,
            InputEvent::MouseWheel { surface_id, .. } => surface_id,
            InputEvent::KeyDown { surface_id, .. } => surface_id,
            InputEvent::KeyUp { surface_id, .. } => surface_id,
            InputEvent::Focus { surface_id, .. } => surface_id,
            InputEvent::CursorEnter { surface_id, .. } => surface_id,
            InputEvent::CursorLeave { surface_id, .. } => surface_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EngineEvent {
    LoadStatus {
        surface_id: String,
        status: String,
        url: Option<String>,
    },
    TitleChanged {
        surface_id: String,
        title: String,
    },
    FramePublished {
        surface_id: String,
        slot_idx: usize,
        pub_seq: u64,
    },
    Pong,
}

