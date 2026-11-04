//! lotus-engine Remote Input & Control Protocol
//!
//! Serialization contract between Host Process (Node / lotus-core) and Secure Engine (lotus-engine).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlMessage {
    Navigate {
        surface_id: String,
        navigation_id: u64,
        url: String,
    },
    Resize {
        surface_id: String,
        generation: u64,
        width: u32,
        height: u32,
        scale_factor: f32,
    },
    Destroy {
        surface_id: String,
    },
    Input {
        event: InputEvent,
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

