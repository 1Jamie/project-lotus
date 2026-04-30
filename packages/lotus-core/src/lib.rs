use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::{env, fs, path::PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;
use std::collections::HashMap;
use std::time::Instant;
use std::num::NonZeroUsize;

use once_cell::sync::OnceCell;

use napi_derive::napi;
use napi::threadsafe_function::{ThreadsafeFunction, ErrorStrategy, ThreadsafeFunctionCallMode};
use serde::{Deserialize, Serialize};
use serde_json;
use uuid::Uuid;
use log::{info, error, debug, trace, warn};

mod window_state;
mod platform;

use window_state::WindowStateManager;

use winit::event::{WindowEvent, MouseScrollDelta};
use winit::event_loop::{EventLoopProxy, ActiveEventLoop, ControlFlow};
use winit::application::ApplicationHandler;
use winit::window::{Window, WindowId, CursorIcon};
use winit::dpi::PhysicalSize as WinitPhysicalSize;
use winit::raw_window_handle::{HasWindowHandle, HasDisplayHandle};
use dpi::PhysicalSize as ServoPhysicalSize;
#[cfg(target_os = "macos")]
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
#[cfg(target_os = "linux")]
use winit::platform::x11::WindowAttributesExtX11;
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;

// Servo Imports
use servo::{
    ServoBuilder, WebViewDelegate,
    WebViewBuilder, WindowRenderingContext, OffscreenRenderingContext, RenderingContext,
    resources::{self, Resource},
    InputEvent, KeyboardEvent as ServoKeyboardEvent,
    Code, Key, KeyState, Location, Modifiers,
    MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    WheelEvent, WheelDelta, WheelMode,
    LoadStatus,
    ConsoleLogLevel,
    WebResourceLoad,
    UserContentManager, UserScript,
};
use euclid::{Point2D, Scale};
use servo::{DeviceIndependentPixel, DevicePixel};
use servo::WebResourceResponse;
use http::header::{HeaderMap, HeaderValue, CONTENT_TYPE};

use http::StatusCode;
use dark_light;
use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce
};
use lru::LruCache;


// IPC Message structure - Removed! process raw bytes.

// Global event loop proxy - initialized once
static EVENT_LOOP_PROXY: OnceCell<EventLoopProxy<EngineCommand>> = OnceCell::new();

// Global app state (thread-safe metadata only, no Rc types)
static APP_STATE: OnceCell<Arc<Mutex<AppState>>> = OnceCell::new();

// Global map of per-window WebSocket senders (for main→renderer pushes)
// Populated by the Axum IPC thread; read by the Winit event-loop thread.
static WS_SENDERS: OnceCell<Arc<dashmap::DashMap<String, tokio::sync::mpsc::UnboundedSender<axum::extract::ws::Message>>>> = OnceCell::new();

// Outgoing message buffer: holds messages for windows whose WS is temporarily down
// (e.g. during a page reload). Drained automatically when the WS reconnects.
// Each entry is a queue of raw msgpack-packed frames, capped to avoid unbounded growth.
static WS_PENDING: OnceCell<Arc<dashmap::DashMap<String, std::collections::VecDeque<Vec<u8>>>>> = OnceCell::new();

// Maximum number of frames to buffer per window while the WS is disconnected.
const WS_PENDING_MAX_FRAMES: usize = 1024;
const MSG_TYPE_CONTROL: u8 = 0x01;
const MSG_TYPE_DATA: u8 = 0x02;

#[cfg(target_os = "linux")]
fn detect_linux_theme_robust() -> dark_light::Mode {
    // 1. Try standard crate
    let mode = dark_light::detect();
    if mode == dark_light::Mode::Dark {
        return mode;
    }

    // 2. Try gsettings for color-scheme (Modern GNOME)
    // gsettings get org.gnome.desktop.interface color-scheme
    if let Ok(output) = Command::new("gsettings")
        .args(&["get", "org.gnome.desktop.interface", "color-scheme"])
        .output() 
    {
        let stdout = String::from_utf8_lossy(&output.stdout).to_lowercase();
        // Output is usually "'prefer-dark'\n"
        if stdout.contains("prefer-dark") {
            return dark_light::Mode::Dark;
        }
    }

    // 3. Try gsettings for gtk-theme (Legacy / Fallback)
    // gsettings get org.gnome.desktop.interface gtk-theme
    if let Ok(output) = Command::new("gsettings")
        .args(&["get", "org.gnome.desktop.interface", "gtk-theme"])
        .output() 
    {
        let stdout = String::from_utf8_lossy(&output.stdout).to_lowercase();
        // Check for "-dark" suffix
        if stdout.contains("-dark") || stdout.contains(":dark") {
            return dark_light::Mode::Dark;
        }
    }

    // Default to what we found initially (Light/Default)
    mode
}

struct ByteLimitedLruCache {
    cache: LruCache<PathBuf, (Vec<u8>, String)>,
    current_bytes: usize,
    max_bytes: usize,
}

impl ByteLimitedLruCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            cache: LruCache::new(NonZeroUsize::new(1000).unwrap()), // Cap at 1000 items as secondary guard
            current_bytes: 0,
            max_bytes,
        }
    }

    fn get(&mut self, key: &PathBuf) -> Option<&(Vec<u8>, String)> {
        self.cache.get(key)
    }

    fn put(&mut self, key: PathBuf, val: (Vec<u8>, String)) {
        let size = val.0.len();
        // If the new item is larger than the total cache size, don't cache it
        if size > self.max_bytes {
            return;
        }

        while self.current_bytes + size > self.max_bytes {
            if let Some((_, (old_data, _))) = self.cache.pop_lru() {
                self.current_bytes -= old_data.len();
            } else {
                break;
            }
        }

        self.current_bytes += size;
        self.cache.put(key, val);
    }
}

struct AutonomousKeyDeriver;

impl AutonomousKeyDeriver {
    fn derive_key() -> Option<[u8; 32]> {
        let shard1 = b"LotusMasterFrameworkShard_v1_2026";
        let shard2 = Self::read_shard_from_binary("LOTUS_APP_S1")?;
        let shard3 = Self::read_shard_from_binary("LOTUS_APP_S2")?;
        
        if shard2.len() != 32 || shard3.len() != 32 {
            eprintln!("[DEBUG] Invalid shard length: s2={}, s3={}", shard2.len(), shard3.len());
            return None;
        }

        let mut key = [0u8; 32];
        for i in 0..32 {
            key[i] = shard2[i] ^ shard3[i] ^ shard1[i % shard1.len()];
        }
        eprintln!("[DEBUG] Key derivation successful.");
        Some(key)
    }

    fn read_shard_from_binary(name: &str) -> Option<Vec<u8>> {
        eprintln!("[DEBUG] Reading shard: {}", name);
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, FindResourceW, LoadResource, LockResource, SizeofResource};

            unsafe {
                let module = GetModuleHandleW(std::ptr::null());
                if module == 0 { return None; }

                // Convert name to UTF-16
                let name_u16: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                // RT_RCDATA is 10
                let rt_rcdata = 10 as *const u16;

                let res = FindResourceW(module, name_u16.as_ptr(), rt_rcdata);
                if res == 0 { return None; }

                let size = SizeofResource(module, res);
                if size == 0 { return None; }

                let handle = LoadResource(module, res);
                if handle.is_null() { return None; }

                let data_ptr = LockResource(handle);
                if data_ptr.is_null() { return None; }

                let mut data = vec![0u8; size as usize];
                std::ptr::copy_nonoverlapping(data_ptr as *const u8, data.as_mut_ptr(), size as usize);
                Some(data)
            }
        }

        #[cfg(target_os = "linux")]
        {
            use object::{Object, ObjectSection};
            use std::fs::File;
            let file = match File::open("/proc/self/exe") {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("[DEBUG] Failed to open /proc/self/exe: {}", e);
                    return None;
                }
            };
            let mmap = unsafe {
                match memmap2::Mmap::map(&file) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("[DEBUG] Failed to map /proc/self/exe: {}", e);
                        return None;
                    }
                }
            };
            let obj_file = match object::File::parse(&*mmap) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("[DEBUG] Failed to parse ELF: {}", e);
                    return None;
                }
            };
            let section = match obj_file.section_by_name(name) {
                Some(s) => s,
                None => {
                    eprintln!("[DEBUG] Section not found: {}", name);
                    return None;
                }
            };
            eprintln!("[DEBUG] Found section {}, size: {}", name, section.size());
            section.data().ok().map(|d| d.to_vec())
        }

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            None
        }
    }
}

struct EncryptedVfs {
    data: Vec<u8>,
    index: HashMap<String, (usize, usize)>, // path -> (offset, size)
    data_offset: usize,
    cipher: Aes256Gcm,
}

impl EncryptedVfs {
    fn init() -> Option<Self> {
        eprintln!("[DEBUG] Initializing EncryptedVfs...");
        let key = match AutonomousKeyDeriver::derive_key() {
            Some(k) => k,
            None => {
                eprintln!("[DEBUG] Key derivation failed in VFS init.");
                return None;
            }
        };
        let cipher = Aes256Gcm::new(&key.into());

        let vfs_data = match AutonomousKeyDeriver::read_shard_from_binary("LOTUS_VFS") {
            Some(d) => d,
            None => {
                eprintln!("[DEBUG] Failed to read VFS blob.");
                return None;
            }
        };
        
        if vfs_data.len() < 12 || &vfs_data[0..8] != b"LOTUSVFS" {
            eprintln!("[DEBUG] Invalid VFS magic or size.");
            return None;
        }
        
        let index_size = u32::from_le_bytes(vfs_data[8..12].try_into().unwrap()) as usize;
        eprintln!("[DEBUG] VFS Index size: {}", index_size);
        let index_json = &vfs_data[12..12+index_size];
        let index: HashMap<String, (usize, usize)> = match serde_json::from_slice(index_json) {
            Ok(idx) => idx,
            Err(e) => {
                eprintln!("[DEBUG] Failed to parse VFS index JSON: {}", e);
                return None;
            }
        };
        
        let data_offset = 12 + index_size;
        eprintln!("[DEBUG] VFS initialized with {} entries.", index.len());
        
        Some(Self {
            data: vfs_data,
            index,
            data_offset,
            cipher,
        })
    }

    fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        let (offset, size) = self.index.get(path)?;
        let encrypted_data = &self.data[self.data_offset + offset .. self.data_offset + offset + size];
        
        // Nonce is prepended to the data block in our VFS format
        if encrypted_data.len() < 12 { return None; }
        let (nonce_bytes, ciphertext) = encrypted_data.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        
        self.cipher.decrypt(nonce, ciphertext).ok()
    }
}

struct AppState {
    window_metadata: HashMap<String, WindowMetadata>,
    window_states: WindowStateManager,
    ipc_server_port: u16,
    ipc_server_token: String,
    msgpackr_source: String,
    profiling: bool,
    _start_time: Instant,
    window_start_times: HashMap<String, Instant>,
    vfs: Option<Arc<EncryptedVfs>>,
    resource_cache: ByteLimitedLruCache,
}

// IPC bootstrap script injected into every page
const IPC_BOOTSTRAP_BASE: &str = r#"
window.lotus = {
    handlers: {},
    _ws: null,
    _offlineQueue: [],
    _batch: [],
    _batchBytes: 0,
    _batchHasControl: false,
    _batchTimeout: null,
    _pendingInvokes: {},
    port: null, // Will be set by init script
    token: null, // Will be set by init script
    id: null,    // Will be set by init script
    _assemblies: {},
    _packer: null,
    _unpacker: null,
    _getPacker: () => {
        if (!window.lotus._packer && window.msgpackr) {
            window.lotus._packer = new window.msgpackr.Packr({ useRecords: false });
        }
        return window.lotus._packer;
    },
    _getUnpacker: () => {
        if (!window.lotus._unpacker && window.msgpackr) {
            window.lotus._unpacker = new window.msgpackr.Unpackr({ useRecords: false });
        }
        return window.lotus._unpacker;
    },
    _handleDecoded: (decodedMsgs) => {
        // Single message unwrapping to match standard IPC emission payload
        if (!Array.isArray(decodedMsgs) || decodedMsgs.length === 0) return;
        
        // Handle Batch format or Array
        // If batch [[channel, payload], [channel, payload]]
        if (Array.isArray(decodedMsgs[0])) {
            for (const [channel, payload] of decodedMsgs) {
                window.lotus.emit(channel, payload);
            }
        } else if (decodedMsgs.length >= 2 && typeof decodedMsgs[0] === 'string') {
            // Single message [channel, payload]
            const channel = decodedMsgs[0];
            const payload = decodedMsgs[1]; // Payload might be undefined if 0 args
            window.lotus.emit(channel, payload);
        }
    },
    _processRaw: (data) => {
        if (!window.msgpackr) return;
        const unpacker = window.lotus._getUnpacker();
        if (!unpacker) return;

        const type = data[0];
        if (type === 0x02) {
            try {
                const decodedMsgs = unpacker.unpack(data.subarray(1));
                window.lotus._handleDecoded(decodedMsgs);
            } catch (e) {
                console.error("Lotus IPC unpack error", e);
            }
        } else if (type === 0x03) {
            window.lotus._handleChunk(data);
        }
    },
    _handleChunk: (data) => {
        if (data.length < 9) return;
        // Protocol: [Type(1)][MsgID(4)][Total(2)][Index(2)][Payload(N)]
        const dv = new DataView(data.buffer, data.byteOffset, data.byteLength);
        const msgId = dv.getUint32(1);
        const total = dv.getUint16(5);
        const index = dv.getUint16(7);
        const payload = data.subarray(9);
        
        if (!window.lotus._assemblies[msgId]) {
            window.lotus._assemblies[msgId] = {
                chunks: new Array(total),
                received: 0,
                lastActivity: Date.now()
            };
        }
        
        const assembly = window.lotus._assemblies[msgId];
        if (!assembly.chunks[index]) {
            assembly.chunks[index] = payload;
            assembly.received++;
            assembly.lastActivity = Date.now();
        }
        
        if (assembly.received === total) {
            delete window.lotus._assemblies[msgId];
            const totalLen = assembly.chunks.reduce((acc, c) => acc + c.length, 0);
            const full = new Uint8Array(totalLen);
            let offset = 0;
            for (const c of assembly.chunks) {
                full.set(c, offset);
                offset += c.length;
            }
            try {
                const unpacker = window.lotus._getUnpacker();
                if (unpacker) {
                    const decodedMsgs = unpacker.unpack(full);
                    window.lotus._handleDecoded(decodedMsgs);
                }
            } catch (e) {
                console.error("Lotus IPC reassembly unpack error", e);
            }
        }
    },

    _connectWs: () => {
       if (window.lotus._ws || !window.lotus.port) return;

       const wsUrl = `ws://127.0.0.1:${window.lotus.port}/ws?token=${window.lotus.token}&id=${window.lotus.id}&paneId=${window.lotus.paneId}`;
       window.lotus._ws = new WebSocket(wsUrl);        window.lotus._ws.binaryType = 'arraybuffer';
        
        window.lotus._ws.onopen = () => {
            // console.log("Lotus IPC WebSocket connected");
            const queue = window.lotus._offlineQueue;
            window.lotus._offlineQueue = [];
            for (const msg of queue) {
                window.lotus._ws.send(msg);
            }
        };

        window.lotus._ws.onclose = () => {
            // console.warn("Lotus IPC WebSocket disconnected, reconnecting in 1s...");
            window.lotus._ws = null;
            setTimeout(window.lotus._connectWs, 1000);
        };

        window.lotus._ws.onerror = (e) => {
            // console.error("Lotus IPC WebSocket error", e);
        };

        window.lotus._ws.onmessage = async (event) => {
            try {
                let data = event.data;
                // If it's a blob, we must await its arrayBuffer to unpack it
                if (data instanceof Blob) {
                    data = await data.arrayBuffer();
                }
                
                if (data instanceof ArrayBuffer) {
                    window.lotus._processRaw(new Uint8Array(data));
                }
            } catch (e) {
                console.error("Lotus IPC message handling error", e);
            }
        };
    },

    send: (channel, data, options = {}) => {
        if (!window.lotus.port) {
            console.error("Lotus IPC port not initialized");
            return;
        }

        // Initialize connection lazily on first send, or explicitly elsewhere
        if (!window.lotus._ws && channel !== "lotus:internal-reconnect") {
            window.lotus._connectWs();
        }

        // All payloads — text, JSON, and binary (Blob/ArrayBuffer/TypedArray) —
        // enter the same batch queue. msgpackr encodes binary entries as msgpack
        // bin naturally, so no special-casing is needed here.
        window.lotus._batch.push([channel, data]);
        
        // SAFE CHECK: Ensure data is defined before accessing byteLength
        const dataLen = data ? (typeof data === 'string' ? data.length * 2 : (data.byteLength || 0)) : 0;
        window.lotus._batchBytes += dataLen;

        if (channel === 'lotus:set-drag-regions') {
            window.lotus._batchHasControl = true;
        }

        const flushBatch = () => {
            if (window.lotus._batch.length === 0) return;
            const batchToFlush = window.lotus._batch;
            const hasControl = window.lotus._batchHasControl;
            window.lotus._batch = [];
            window.lotus._batchBytes = 0;
            window.lotus._batchHasControl = false;
            window.lotus._batchTimeout = null;

            if (window.msgpackr) {
                const packer = window.lotus._getPacker();
                if (!packer) return;
                try {
                    const packed = packer.pack(batchToFlush);
                    const CHUNK_THRESHOLD = 1024 * 1024; // 1MB
                    if (packed.length > CHUNK_THRESHOLD) {
                        window.lotus._chunkAndSend(packed, hasControl);
                    } else {
                        const finalPayload = new Uint8Array(packed.length + 1);
                        finalPayload[0] = hasControl ? 0x01 : 0x02;
                        finalPayload.set(packed, 1);
                        window.lotus._rawSend(finalPayload);
                    }
                } catch (e) {
                    console.error("Failed to pack batch", e);
                }
            } else {
                console.error("msgpackr not loaded");
            }
        };

        window.lotus._chunkAndSend = (packed, hasControl) => {
            const CHUNK_SIZE = 128 * 1024;
            const total = Math.ceil(packed.length / CHUNK_SIZE);
            const msgId = (Math.random() * 0xFFFFFFFF) >>> 0;
            
            let index = 0;
            const sendNext = () => {
                if (index >= total) return;
                
                const start = index * CHUNK_SIZE;
                const end = Math.min(start + CHUNK_SIZE, packed.length);
                const chunkPayload = packed.subarray(start, end);
                
                const final = new Uint8Array(chunkPayload.length + 9);
                final[0] = 0x03;
                const dv = new DataView(final.buffer);
                dv.setUint32(1, msgId);
                dv.setUint16(5, total);
                dv.setUint16(7, index);
                final.set(chunkPayload, 9);
                
                window.lotus._rawSend(final);
                index++;
                if (index < total) {
                    // Use setTimeout in the Renderer to ensure the event loop yields
                    // to rendering/composition between chunks.
                    setTimeout(sendNext, 0);
                }
            };
            sendNext();
        };

        window.lotus._rawSend = (data) => {
            if (window.lotus._ws && window.lotus._ws.readyState === WebSocket.OPEN) {
                window.lotus._ws.send(data);
            } else {
                window.lotus._offlineQueue.push(data);
            }
        };

        // Eager flush to pipeline large bursts instead of packing 100 MB at once
        if (options.urgent || window.lotus._batch.length >= 800 || window.lotus._batchBytes >= 1024 * 1024) {
            flushBatch();
        } else if (!window.lotus._batchTimeout) {
            queueMicrotask(flushBatch);
            window.lotus._batchTimeout = true;
        }
    },
    invoke: (channel, data) => {
        return new Promise((resolve, reject) => {
            const replyId = 'lotus:reply:' + Math.random().toString(36).slice(2) + Date.now().toString(36);
            const timeoutMs = 30000;
            const timer = setTimeout(() => {
                delete window.lotus._pendingInvokes[replyId];
                reject(new Error(`lotus.invoke('${channel}') timed out after ${timeoutMs}ms`));
            }, timeoutMs);
            window.lotus._pendingInvokes[replyId] = (result) => {
                clearTimeout(timer);
                delete window.lotus._pendingInvokes[replyId];
                if (result && result._error !== undefined) {
                    reject(new Error(result._error));
                } else {
                    resolve(result);
                }
            };
            // Guard: if data is null/undefined/primitive, spread nothing rather than throwing.
            // Objects are spread normally so all existing keys are preserved.
            const payload = Object.assign(
                {},
                (data !== null && typeof data === 'object') ? data : {},
                { _replyId: replyId }
            );
            window.lotus.send(channel, payload);
        });
    },
    on: (channel, handler) => {
        if (!window.lotus.handlers[channel]) window.lotus.handlers[channel] = [];
        window.lotus.handlers[channel].push(handler);
    },
    off: (channel, handler) => {
        if (window.lotus.handlers[channel]) {
            const index = window.lotus.handlers[channel].indexOf(handler);
            if (index !== -1) window.lotus.handlers[channel].splice(index, 1);
        }
    },
    emit: (channel, data) => {
        // Route replies to pending invoke() calls first
        if (channel.startsWith('lotus:reply:')) {
            const cb = window.lotus._pendingInvokes[channel];
            if (cb) { cb(data); }
            return;
        }
        (window.lotus.handlers[channel] || []).forEach(h => h(data));
    }
};
"#;

const DRAG_REGION_SCRIPT: &str = r#"
(function() {
    let updateTimeout = null;
    function updateDragRegions() {
        if (updateTimeout) clearTimeout(updateTimeout);
        updateTimeout = setTimeout(() => {
            const dragRects = [];
            const noDragRects = [];
            const dpr = window.devicePixelRatio || 1;
            
            // Support both data attributes (Lotus standard) and computed CSS (Electron compatibility)
            // We iterate over everything that might be a region to ensure CSS support.
            const candidates = document.querySelectorAll('*');
            candidates.forEach(el => {
                const style = getComputedStyle(el);
                const region = style.webkitAppRegion;
                const isDragAttr = el.getAttribute('data-lotus-drag') === 'true';
                const isNoDragAttr = el.getAttribute('data-lotus-drag') === 'false';

                if (region === 'drag' || isDragAttr) {
                    const rect = el.getBoundingClientRect();
                    const dpr = window.devicePixelRatio || 1;
                    dragRects.push({ x: rect.x * dpr, y: rect.y * dpr, width: rect.width * dpr, height: rect.height * dpr });
                } else if (region === 'no-drag' || isNoDragAttr) {
                    const rect = el.getBoundingClientRect();
                    const dpr = window.devicePixelRatio || 1;
                    noDragRects.push({ x: rect.x * dpr, y: rect.y * dpr, width: rect.width * dpr, height: rect.height * dpr });
                }
            });
            
            if (window.lotus && window.lotus.send) {
                window.lotus.send('lotus:set-drag-regions', { 
                    paneId: window.lotus.paneId, 
                    drag: dragRects, 
                    noDrag: noDragRects 
                });
            }
        }, 32); // Slightly higher debounce for full-tree scan
    }

    function initObservers() {
        if (!document.body) return;
        
        const observer = new MutationObserver((mutations) => {
            let shouldUpdate = false;
            for (const mutation of mutations) {
                if (mutation.type === 'childList') {
                    shouldUpdate = true;
                    break;
                }
                if (mutation.type === 'attributes') {
                    shouldUpdate = true; // Any attribute change might affect style/drag
                    break;
                }
            }
            if (shouldUpdate) updateDragRegions();
        });

        observer.observe(document.body, { 
            childList: true, 
            subtree: true, 
            attributes: true
        });

        const resizeObserver = new ResizeObserver(updateDragRegions);
        resizeObserver.observe(document.body);
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', () => {
            updateDragRegions();
            initObservers();
        });
    } else {
        updateDragRegions();
        initObservers();
    }
})();
"#;

const STENCIL_VS: &str = r#"
#version 330 core
const vec2 verts[4] = vec2[4](vec2(-1.0, -1.0), vec2(1.0, -1.0), vec2(-1.0, 1.0), vec2(1.0, 1.0));
out vec2 v_pos;
void main() {
    vec2 pos = verts[gl_VertexID];
    v_pos = pos;
    gl_Position = vec4(pos, 0.0, 1.0);
}
"#;

const STENCIL_FS: &str = r#"
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

const COMP_VS: &str = r#"
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

const COMP_FS: &str = r#"
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

struct WindowMetadata {
    root_path: Option<PathBuf>,
    last_window_size: Option<winit::dpi::PhysicalSize<u32>>,
}

fn map_winit_modifiers(winit_mods: winit::event::Modifiers) -> Modifiers {
    let mut s_mods = Modifiers::empty();
    let state = winit_mods.state();
    if state.shift_key() { s_mods.insert(Modifiers::SHIFT); }
    if state.control_key() { s_mods.insert(Modifiers::CONTROL); }
    if state.alt_key() { s_mods.insert(Modifiers::ALT); }
    if state.super_key() { s_mods.insert(Modifiers::META); }
    s_mods
}

fn map_winit_key(winit_key: &winit::keyboard::Key) -> Key {
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

fn map_winit_code(winit_code: winit::keyboard::PhysicalKey) -> Code {
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

use glow::HasContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneAnchor {
    None = 0,
    Fill = 1,
    Left = 2,
    Right = 3,
    Top = 4,
    Bottom = 5,
}

impl From<u32> for PaneAnchor {
    fn from(v: u32) -> Self {
        match v {
            1 => PaneAnchor::Fill,
            2 => PaneAnchor::Left,
            3 => PaneAnchor::Right,
            4 => PaneAnchor::Top,
            5 => PaneAnchor::Bottom,
            _ => PaneAnchor::None,
        }
    }
}

struct PaneInstance {
    id: String,
    webview: servo::WebView,
    rect: euclid::Rect<f32, servo::DeviceIndependentPixel>,
    last_notified_rect: Option<euclid::Rect<f32, servo::DeviceIndependentPixel>>,
    last_physical_rect: euclid::default::Rect<i32>,
    z_index: i32,
    anchor: PaneAnchor,
    dock_order: u32,
    animating: bool,
    is_dirty: bool,
    needs_repaint: bool,
    is_visible: bool,
    drag_regions: Vec<euclid::Rect<f32, servo::DevicePixel>>,
    no_drag_regions: Vec<euclid::Rect<f32, servo::DevicePixel>>,
    // Each pane gets its own OffscreenRenderingContext so Servo renders it into
    // a dedicated FBO. We then blit each FBO to the window at the correct position.
    offscreen_ctx: Rc<OffscreenRenderingContext>,
    // Resize throttle: Servo can only handle one resize at a time.
    // servo_busy is true while Servo is mid-render (webview.resize called,
    // NewFrameReady not yet received).  When a new SetPaneRect arrives while
    // busy, we store the desired size in pending_servo_size (queue-of-1,
    // overwriting any previous pending) and apply it the instant Servo is free.
    // current_servo_size is the size we last actually sent to webview.resize();
    // the FBO must stay in sync with this, not with pane.rect, so we never
    // clear the FBO while a render is in-flight.
    servo_busy: bool,
    pending_servo_size: Option<ServoPhysicalSize<u32>>,
    requested_servo_size: ServoPhysicalSize<u32>,
    current_servo_size: ServoPhysicalSize<u32>,
    pub ready_frame_size: Option<ServoPhysicalSize<u32>>,
    pub ready_to_repaint: Arc<AtomicBool>,
    pub first_frame_painted: bool,
    pub first_frame_painted_time: Option<std::time::Instant>,
    pub pending_physical_rect: Option<euclid::default::Rect<i32>>,
    pub is_resizing: bool,
    pub pending_visible: bool,
    pub frames_until_stable: u8,
    pub ghost_tex: Option<glow::NativeTexture>,
    pub ghost_tex_size: Option<winit::dpi::PhysicalSize<u32>>,
}

#[derive(Debug, Clone)]
struct StagedPaneLayout {
    rect: euclid::default::Rect<i32>,
    visible: bool,
}

#[derive(Debug, Clone, Default)]
struct StagedLayout {
    panes: HashMap<String, StagedPaneLayout>,
    width: u32,
    height: u32,
    scale_factor: f32,
}

struct WindowInstance {
    pub panes: HashMap<String, PaneInstance>,
    pub active_layout: StagedLayout,
    pub staged_layout: StagedLayout,
    pub active_pane_id: String,
    pub primary_pane_id: String, // Target for legacy win.loadUrl() calls
    pub last_mouse_down_pane_id: Option<String>,
    rendering_context: Rc<WindowRenderingContext>,
    gl: Arc<glow::Context>,
    window: Arc<Window>,
    last_mouse_pos: Point2D<f32, servo::DevicePixel>,
    is_mouse_down: bool,
    modifiers: Modifiers,
    frameless: bool,
    transparent: bool,
    in_resize_border: bool,
    auto_resize_main: bool,
    pub corner_radius: Option<f64>,
    // Keep dummies alive to force unique IDs for WebViews in the shared Servo instance.
    id_shifters: Vec<servo::WebView>,
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
                    available_phys.origin.y += clamped_h;
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
                } else {
                    // Position shift only: Join the flip but don't hold it back
                    pane.frames_until_stable = 0;
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

        // --- ANTI-DEADLOCK DISPATCHER ---
        // Forces Servo to render exactly what the locked transaction is waiting for.
        for pane in self.panes.values_mut() {
            if !pane.servo_busy {
                if let Some(committed) = &self.committed_layout {
                    if let Some(staged) = committed.panes.get(&pane.id) {
                        let target_size = ServoPhysicalSize::new(
                            (staged.rect.size.width as u32).max(1),
                            (staged.rect.size.height as u32).max(1)
                        );
                        
                        // THE FIX: We must kickstart Servo if it has no ghost snapshot, 
                        // or if its last finished frame doesn't match the transaction target.
                        if pane.current_servo_size != target_size || pane.ghost_tex_size.is_none() {
                            pane.webview.resize(target_size);
                            pane.requested_servo_size = target_size;
                            pane.servo_busy = true;
                            pane.pending_servo_size = None; // Wipe any spurious queued sizes
                        }
                    }
                }
            }
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
        }
    }
}

impl PaneInstance {
}

fn dispatch_to_renderer(window_id: String, pane_id: Option<String>, data: axum::body::Bytes) {
    let msg = axum::extract::ws::Message::Binary(data.to_vec());
    if let Some(senders) = WS_SENDERS.get() {
        match pane_id {
            Some(p) => {
                let client_id = format!("{}:{}", window_id, p);
                let alt_client_id = if p == "main" { Some(window_id.clone()) } else { None };
                
                let maybe_tx = senders.get(&client_id)
                    .or_else(|| alt_client_id.as_ref().and_then(|id| senders.get(id)));

                if let Some(tx) = maybe_tx {
                    if tx.send(msg).is_err() {
                        if let Some(pending) = WS_PENDING.get() {
                            let mut q = pending.entry(client_id).or_insert_with(std::collections::VecDeque::new);
                            if q.len() < WS_PENDING_MAX_FRAMES {
                                q.push_back(data.to_vec());
                            }
                        }
                    }
                } else {
                    // Not connected yet — queue it
                    if let Some(pending) = WS_PENDING.get() {
                        let mut q = pending.entry(client_id).or_insert_with(std::collections::VecDeque::new);
                        if q.len() < WS_PENDING_MAX_FRAMES {
                            q.push_back(data.to_vec());
                        }
                    }
                }
            },
            None => {
                // Broadcast to all panes of this window
                let mut found_any = false;
                for entry in senders.iter() {
                    if entry.key().starts_with(&format!("{}:", window_id)) || entry.key() == &window_id {
                        let _ = entry.value().send(msg.clone());
                        found_any = true;
                    }
                }
                
                if !found_any {
                    // Fallback to window-level queue if no panes are connected
                    if let Some(pending) = WS_PENDING.get() {
                        let mut q = pending.entry(window_id).or_insert_with(std::collections::VecDeque::new);
                        if q.len() < WS_PENDING_MAX_FRAMES {
                            q.push_back(data.to_vec());
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum EngineCommand {
    Wake,
    // App-level commands
    CreateWindow(WindowOptions, String), // options, window_id
    Quit,

    // Window-specific commands (all take window ID)
    LoadUrl(String, String, String), // window_id, pane_id, url
    IpcMessage(String, Vec<u8>), // window_id, raw_bytes
    IpcMessages(String, Vec<Vec<u8>>),
    Resize(String, winit::dpi::PhysicalSize<u32>), // window_id, size
    SetPosition(String, winit::dpi::PhysicalPosition<i32>), // window_id, position
    SetAlwaysOnTop(String, bool), // window_id, flag
    RequestAttention(String), // window_id
    SetTitle(String, String), // window_id, title
    CloseWindow(String), // window_id
    SetDecorations(String, bool), // window_id, decorations
    ExecuteScript(String, String, String), // window_id, pane_id, script
    ShowWindow(String), // window_id
    HideWindow(String), // window_id
    UpdateDragRegions(String, String, Vec<euclid::Rect<f32, servo::DevicePixel>>, Vec<euclid::Rect<f32, servo::DevicePixel>>), // window_id, pane_id, drag_regions, no_drag_regions
    MinimizeWindow(String), // window_id
    UnminimizeWindow(String), // window_id
    MaximizeWindow(String), // window_id
    UnmaximizeWindow(String), // window_id
    FocusWindow(String), // window_id
    AnimatingChanged(String, String, bool), // window_id, pane_id, animating
    NewFrameReady(String, String), // window_id, pane_id
    SetMinInnerSize(String, Option<winit::dpi::PhysicalSize<u32>>), // window_id, size (None = remove constraint)
    SetMaxInnerSize(String, Option<winit::dpi::PhysicalSize<u32>>), // window_id, size (None = remove constraint)

    // Pane-specific commands
    CreatePane(String, String, String, euclid::Rect<f32, servo::DeviceIndependentPixel>, i32, PaneAnchor, u32), // window_id, pane_id, url, rect, z_index, anchor, dock_order
    RemovePane(String, String), // window_id, pane_id
    SetPaneRect(String, String, euclid::Rect<f32, servo::DeviceIndependentPixel>), // window_id, pane_id, rect
    SetPaneVisible(String, String, bool), // window_id, pane_id, visible
    FocusPane(String, String), // window_id, pane_id
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

fn intercept_drag_regions(raw_bytes: &[u8], window_id: String) {
    let mut cursor = std::io::Cursor::new(raw_bytes);
    match rmpv::decode::value::read_value(&mut cursor) {
        Ok(batch_val) => {
            trace!("Rust: Intercepted IPC batch value: {:?}", batch_val);
            if let Some(batch) = batch_val.as_array() {
                if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                    // Extract true window_id if it's a composite clientId (window_id:pane_id)
                    let true_window_id = window_id.split(':').next().unwrap_or(&window_id).to_string();

                    for msg in batch {
                        if let Some(pair) = msg.as_array() {
                            if pair.len() >= 2 {
                                if let Some(channel) = pair[0].as_str() {
                                    if channel == "lotus:set-drag-regions" {
                                        match rmpv::ext::from_value::<DragRegionPayload>(pair[1].clone()) {
                                            Ok(payload) => {
                                                let drag_regions = payload.drag.unwrap_or_default().into_iter().map(|r| {
                                                    euclid::Rect::new(
                                                        euclid::Point2D::new(r.x, r.y),
                                                        euclid::Size2D::new(r.width, r.height)
                                                    )
                                                }).collect::<Vec<_>>();
                                                
                                                let no_drag_regions = payload.no_drag.unwrap_or_default().into_iter().map(|r| {
                                                    euclid::Rect::new(
                                                        euclid::Point2D::new(r.x, r.y),
                                                        euclid::Size2D::new(r.width, r.height)
                                                    )
                                                }).collect::<Vec<_>>();
                                                
                                                let pid = payload.pane_id.unwrap_or_else(|| "main".to_string());
                                                info!("Rust: Intercepted {} drag regions for pane '{}' in window '{}': {:?}", drag_regions.len(), pid, true_window_id, drag_regions);
                                                
                                                let _ = proxy.send_event(EngineCommand::UpdateDragRegions(
                                                    true_window_id.clone(),
                                                    pid,
                                                    drag_regions,
                                                    no_drag_regions
                                                ));
                                            },
                                            Err(e) => {
                                                debug!("Rust: Failed to deserialize DragRegionPayload: {:?}", e);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
        Err(e) => {
            trace!("Rust: Failed to decode IPC batch as rmpv::Value: {:?}", e);
        }
    }
}

#[napi]
pub struct WindowHandle {
    id: String,
}

#[napi]
impl WindowHandle {
    #[napi]
    pub fn get_id(&self) -> String {
        self.id.clone()
    }

    #[napi]
    pub fn load_url(&self, url: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::LoadUrl(self.id.clone(), "main".to_string(), url));
        }
    }

    #[napi]
    pub fn load_url_in_pane(&self, pane_id: String, url: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::LoadUrl(self.id.clone(), pane_id, url));
        }
    }

    #[napi]
    pub fn send_to_renderer(&self, data: napi::bindgen_prelude::Buffer) -> napi::Result<()> {
        dispatch_to_renderer(self.id.clone(), None, axum::body::Bytes::from(data.to_vec()));
        Ok(())
    }

    #[napi]
    pub fn send_to_pane_renderer(&self, pane_id: String, data: napi::bindgen_prelude::Buffer) -> napi::Result<()> {
        dispatch_to_renderer(self.id.clone(), Some(pane_id), axum::body::Bytes::from(data.to_vec()));
        Ok(())
    }

    #[napi]
    pub fn resize(&self, width: u32, height: u32) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::Resize(
                self.id.clone(),
                winit::dpi::PhysicalSize::new(width, height)
            ));
        }
    }

    #[napi]
    pub fn set_position(&self, x: i32, y: i32) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetPosition(
                self.id.clone(),
                winit::dpi::PhysicalPosition::new(x, y)
            ));
        }
    }

    #[napi]
    pub fn show(&self) {
         if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::ShowWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn hide(&self) {
         if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::HideWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn set_always_on_top(&self, always_on_top: bool) {

        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetAlwaysOnTop(self.id.clone(), always_on_top));
        }
    }

    #[napi]
    pub fn request_attention(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::RequestAttention(self.id.clone()));
        }
    }

    #[napi]
    pub fn set_title(&self, title: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetTitle(self.id.clone(), title));
        }
    }

    #[napi]
    pub fn set_decorations(&self, decorations: bool) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetDecorations(self.id.clone(), decorations));
        }
    }

    #[napi]
    pub fn close(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::CloseWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn update_drag_regions(&self, rects_json: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            if let Ok(data) = serde_json::from_str::<DragRegionPayload>(&rects_json) {
                let drag_regions = data.drag.unwrap_or_default().into_iter().map(|r| {
                    euclid::Rect::new(
                        euclid::Point2D::new(r.x, r.y),
                        euclid::Size2D::new(r.width, r.height)
                    )
                }).collect::<Vec<_>>();

                let no_drag_regions = data.no_drag.unwrap_or_default().into_iter().map(|r| {
                    euclid::Rect::new(
                        euclid::Point2D::new(r.x, r.y),
                        euclid::Size2D::new(r.width, r.height)
                    )
                }).collect::<Vec<_>>();

                debug!("Rust: Updated drag regions for window {}: drag: {}, no_drag: {}", self.id, drag_regions.len(), no_drag_regions.len());
                let _ = proxy.send_event(EngineCommand::UpdateDragRegions(self.id.clone(), "main".to_string(), drag_regions, no_drag_regions));
            }
        }
    }

    #[napi]
    pub fn update_pane_drag_regions(&self, pane_id: String, rects_json: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            if let Ok(data) = serde_json::from_str::<DragRegionPayload>(&rects_json) {
                let drag_regions = data.drag.unwrap_or_default().into_iter().map(|r| {
                    euclid::Rect::new(
                        euclid::Point2D::new(r.x, r.y),
                        euclid::Size2D::new(r.width, r.height)
                    )
                }).collect::<Vec<_>>();

                let no_drag_regions = data.no_drag.unwrap_or_default().into_iter().map(|r| {
                    euclid::Rect::new(
                        euclid::Point2D::new(r.x, r.y),
                        euclid::Size2D::new(r.width, r.height)
                    )
                }).collect::<Vec<_>>();

                let _ = proxy.send_event(EngineCommand::UpdateDragRegions(self.id.clone(), pane_id, drag_regions, no_drag_regions));
            }
        }
    }

    #[napi]
    pub fn execute_script(&self, script: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::ExecuteScript(self.id.clone(), "main".to_string(), script));
        }
    }

    #[napi]
    pub fn execute_script_in_pane(&self, pane_id: String, script: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::ExecuteScript(self.id.clone(), pane_id, script));
        }
    }

    #[napi]
    pub fn create_pane(&self, pane_id: String, url: String, x: f64, y: f64, width: f64, height: f64, z_index: i32, anchor: Option<u32>, dock_order: Option<u32>) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let rect: euclid::Rect<f32, servo::DeviceIndependentPixel> = euclid::Rect::new(euclid::Point2D::new(x as f32, y as f32), euclid::Size2D::new(width as f32, height as f32));
            let _ = proxy.send_event(EngineCommand::CreatePane(
                self.id.clone(), 
                pane_id, 
                url, 
                rect, 
                z_index, 
                PaneAnchor::from(anchor.unwrap_or(0)), 
                dock_order.unwrap_or(0)
            ));
        }
    }

    #[napi]
    pub fn remove_pane(&self, pane_id: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::RemovePane(self.id.clone(), pane_id));
        }
    }

    #[napi]
    pub fn set_pane_rect(&self, pane_id: String, x: f64, y: f64, width: f64, height: f64) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let rect: euclid::Rect<f32, servo::DeviceIndependentPixel> = euclid::Rect::new(euclid::Point2D::new(x as f32, y as f32), euclid::Size2D::new(width as f32, height as f32));
            let _ = proxy.send_event(EngineCommand::SetPaneRect(self.id.clone(), pane_id, rect));
        }
    }

    #[napi]
    pub fn set_pane_visible(&self, pane_id: String, visible: bool) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::SetPaneVisible(self.id.clone(), pane_id, visible));
        }
    }

    #[napi]
    pub fn focus_pane(&self, pane_id: String) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::FocusPane(self.id.clone(), pane_id));
        }
    }

    #[napi]
    pub fn minimize(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::MinimizeWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn unminimize(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::UnminimizeWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn maximize(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::MaximizeWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn unmaximize(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::UnmaximizeWindow(self.id.clone()));
        }
    }

    #[napi]
    pub fn focus(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::FocusWindow(self.id.clone()));
        }
    }

    /// Set the minimum inner size the user can resize the window to.
    /// Pass 0 for both width and height to remove the constraint.
    #[napi]
    pub fn set_min_size(&self, width: u32, height: u32) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let size = if width == 0 && height == 0 {
                None
            } else {
                Some(winit::dpi::PhysicalSize::new(width, height))
            };
            let _ = proxy.send_event(EngineCommand::SetMinInnerSize(self.id.clone(), size));
        }
    }

    /// Set the maximum inner size the user can resize the window to.
    /// Pass 0 for both width and height to remove the constraint.
    #[napi]
    pub fn set_max_size(&self, width: u32, height: u32) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let size = if width == 0 && height == 0 {
                None
            } else {
                Some(winit::dpi::PhysicalSize::new(width, height))
            };
            let _ = proxy.send_event(EngineCommand::SetMaxInnerSize(self.id.clone(), size));
        }
    }
}




// ------------------------------------------------------------------
// RESOURCES IMPLEMENTATION
// ------------------------------------------------------------------

struct ResourceReader;

impl resources::ResourceReaderMethods for ResourceReader {
    fn read(&self, file: Resource) -> Vec<u8> {
        let mut path = resources_dir_path().clone();
        path.push(file.filename());
        // debug!("Rust: Reading resource: {:?}", path); 
        match fs::read(&path) {
            Ok(data) => data,
            Err(e) => {
                eprintln!("Rust Warning: Missing resource: {:?} ({})", path, e);
                Vec::new()
            }
        }
    }
    fn sandbox_access_files_dirs(&self) -> Vec<PathBuf> {
        vec![resources_dir_path().clone()]
    }
    fn sandbox_access_files(&self) -> Vec<PathBuf> {
        vec![]
    }
}

static RESOURCE_READER: ResourceReader = ResourceReader;
servo::submit_resource_reader!(&RESOURCE_READER);

static RESOURCES_DIR: once_cell::sync::Lazy<PathBuf> = once_cell::sync::Lazy::new(|| {
    let mut path = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    path.push("resources");
    if path.exists() {
        return path;
    }
    // Fallback?
    path.pop();
    path
});

fn resources_dir_path() -> &'static PathBuf {
    &RESOURCES_DIR
}

// ------------------------------------------------------------------
// DELEGATE IMPLEMENTATIONS
// ------------------------------------------------------------------


struct LotusPaneDelegate {
    window: Arc<Window>,
    window_id: String,
    pane_id: String,
    proxy: EventLoopProxy<EngineCommand>,
    ready_to_repaint: Arc<AtomicBool>,
}

impl WebViewDelegate for LotusPaneDelegate {
    fn notify_load_status_changed(&self, _webview: servo::WebView, status: LoadStatus) {
        let (profiling, window_start) = if let Some(state) = APP_STATE.get() {
            if let Ok(s) = state.lock() {
                (s.profiling, s.window_start_times.get(&self.window_id).cloned())
            } else {
                (false, None)
            }
        } else {
            (false, None)
        };

        if profiling {
            if let Some(start) = window_start {
                let elapsed = start.elapsed();
                eprintln!("[PROFILE] Window {} Pane {} status {:?} reached in {:?}", self.window_id, self.pane_id, status, elapsed);
            }
        }

        info!("Rust: LoadStatus changed to {:?} for {} pane {}", status, self.window_id, self.pane_id);
        
        let status_str = match status {
            LoadStatus::Started => "started",
            LoadStatus::HeadParsed => "head-parsed",
            LoadStatus::Complete => "complete",
        };

        if status == LoadStatus::Started {
            let client_id = format!("{}:{}", self.window_id, self.pane_id);
            if let Some(pending) = WS_PENDING.get() {
                if pending.contains_key(&client_id) {
                    info!("Rust: Purging WS_PENDING for {} due to LoadStatus::Started", client_id);
                    pending.remove(&client_id);
                }
            }
        }

        if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
            "event": "load-status",
            "window_id": self.window_id,
            "pane_id": self.pane_id,
            "status": status_str
        })) {
            if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                let _ = proxy.send_event(EngineCommand::IpcMessage(self.window_id.clone(), msg));
            }
        }
    }
    
    fn notify_new_frame_ready(&self, _webview: servo::WebView) {
        trace!("Rust: [Lifecycle] notify_new_frame_ready for window {} pane {}", self.window_id, self.pane_id);
        if self.ready_to_repaint.swap(false, Ordering::SeqCst) {
            let _ = self.proxy.send_event(EngineCommand::NewFrameReady(self.window_id.clone(), self.pane_id.clone()));
        }
    }

    fn notify_animating_changed(&self, _webview: servo::WebView, animating: bool) {
        trace!("Rust: Animating changed for {} pane {} -> {}", self.window_id, self.pane_id, animating);
        let _ = self.proxy.send_event(EngineCommand::AnimatingChanged(self.window_id.clone(), self.pane_id.clone(), animating));
    }
    
    fn notify_page_title_changed(&self, _webview: servo::WebView, title: Option<String>) {
         info!("Rust: Title changed for {} pane {} to {:?}", self.window_id, self.pane_id, title);
         if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
            "event": "title-changed",
            "window_id": self.window_id,
            "pane_id": self.pane_id,
            "title": title
        })) {
            if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                let _ = proxy.send_event(EngineCommand::IpcMessage(self.window_id.clone(), msg));
            }
        }
    }

    fn show_console_message(&self, _webview: servo::WebView, _level: ConsoleLogLevel, message: String) {
        info!("Rust Console [{}|{}]: {}", self.window_id, self.pane_id, message);
    }

    fn load_web_resource(&self, _webview: servo::WebView, load: WebResourceLoad) {
        let url = load.request().url.clone();
        let url_str = url.as_str();
        
        if url_str.starts_with("lotus-resource://") {
            let path_in_url = url.path();
            let relative_path = path_in_url.trim_start_matches('/');
            let path_buf = PathBuf::from(relative_path);

            // 1. Check LRU Cache
            if let Some(state) = APP_STATE.get() {
                if let Ok(mut s) = state.lock() {
                    if let Some((data, mime_str)) = s.resource_cache.get(&path_buf) {
                        debug!("Rust: Cache hit for {:?}", path_buf);
                        let mut headers = HeaderMap::new();
                        if let Ok(val) = HeaderValue::from_str(mime_str) {
                             headers.insert(CONTENT_TYPE, val);
                        }
                        let response = WebResourceResponse::new(url)
                            .headers(headers)
                            .status_code(StatusCode::OK);
                        let mut intercepted = load.intercept(response);
                        intercepted.send_body_data(data.clone());
                        intercepted.finish();
                        return;
                    }
                }
            }

            // 2. Check VFS
            let mut vfs_data = None;
            if let Some(state) = APP_STATE.get() {
                if let Ok(s) = state.lock() {
                    if let Some(vfs) = &s.vfs {
                        vfs_data = vfs.read_file(relative_path);
                    }
                }
            }

            if let Some(data) = vfs_data {
                debug!("Rust: Loaded from VFS: {}", relative_path);
                let mime = mime_guess::from_path(relative_path).first_or_octet_stream();
                let mime_str = mime.to_string();
                
                // Cache it
                if let Some(state) = APP_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        s.resource_cache.put(path_buf.clone(), (data.clone(), mime_str.clone()));
                    }
                }

                let mut headers = HeaderMap::new();
                if let Ok(val) = HeaderValue::from_str(&mime_str) {
                     headers.insert(CONTENT_TYPE, val);
                }
                let response = WebResourceResponse::new(url)
                    .headers(headers)
                    .status_code(StatusCode::OK);
                let mut intercepted = load.intercept(response);
                intercepted.send_body_data(data);
                intercepted.finish();
                return;
            }

             // 3. Fallback to physical filesystem
             let root_path = if let Some(state) = APP_STATE.get() {
                 if let Ok(s) = state.lock() {
                     s.window_metadata.get(&self.window_id)
                        .and_then(|m| m.root_path.clone())
                 } else {
                     None
                 }
             } else {
                 None
             };

             if let Some(root) = root_path {
                 let full_path = root.join(relative_path);
                 
                 // Security: Prevent directory traversal attacks.
                 match (full_path.canonicalize(), root.canonicalize()) {
                     (Ok(canonical_full), Ok(canonical_root)) => {
                         if !canonical_full.starts_with(&canonical_root) {
                             warn!("Rust: Blocked directory traversal attempt for {:?}", full_path);
                             let response = WebResourceResponse::new(url)
                                 .status_code(StatusCode::FORBIDDEN);
                             load.intercept(response).finish();
                             return;
                         }
                         // Path is safe — serve it
                         debug!("Rust: Loading resource from disk: {:?}", canonical_full);
                         match fs::read(&canonical_full) {
                             Ok(data) => {
                                 let mime = mime_guess::from_path(&canonical_full).first_or_octet_stream();
                                 let mime_str = mime.to_string();
                                 
                                 // Cache it
                                 if let Some(state) = APP_STATE.get() {
                                     if let Ok(mut s) = state.lock() {
                                         s.resource_cache.put(path_buf.clone(), (data.clone(), mime_str.clone()));
                                     }
                                 }

                                 let mut headers = HeaderMap::new();
                                 if let Ok(val) = HeaderValue::from_str(&mime_str) {
                                      headers.insert(CONTENT_TYPE, val);
                                 }
                                 let response = WebResourceResponse::new(url)
                                     .headers(headers)
                                     .status_code(StatusCode::OK);

                                 let mut intercepted = load.intercept(response);
                                 intercepted.send_body_data(data);
                                 intercepted.finish();
                             },
                             Err(e) => {
                                 error!("Failed to read file {:?}: {}", canonical_full, e);
                                 let response = WebResourceResponse::new(url)
                                     .status_code(StatusCode::NOT_FOUND);
                                 load.intercept(response).finish();
                             }
                         }
                     },
                     _ => {
                         debug!("Rust: Resource not found: {:?}", full_path);
                         let response = WebResourceResponse::new(url)
                             .status_code(StatusCode::NOT_FOUND);
                         load.intercept(response).finish();
                     }
                 }
                 return;
             }
        }
        
        // For all other URLs, don't intercept (let default handling occur)
    }

    fn notify_cursor_changed(&self, _webview: servo::WebView, cursor: servo::Cursor) {
        let winit_cursor = match cursor {
            servo::Cursor::Default => CursorIcon::Default,
            servo::Cursor::Pointer => CursorIcon::Pointer,
            servo::Cursor::ContextMenu => CursorIcon::ContextMenu,
            servo::Cursor::Help => CursorIcon::Help,
            servo::Cursor::Progress => CursorIcon::Progress,
            servo::Cursor::Wait => CursorIcon::Wait,
            servo::Cursor::Cell => CursorIcon::Cell,
            servo::Cursor::Crosshair => CursorIcon::Crosshair,
            servo::Cursor::Text => CursorIcon::Text,
            servo::Cursor::VerticalText => CursorIcon::VerticalText,
            servo::Cursor::Alias => CursorIcon::Alias,
            servo::Cursor::Copy => CursorIcon::Copy,
            servo::Cursor::Move => CursorIcon::Move,
            servo::Cursor::NoDrop => CursorIcon::NoDrop,
            servo::Cursor::NotAllowed => CursorIcon::NotAllowed,
            servo::Cursor::Grab => CursorIcon::Grab,
            servo::Cursor::Grabbing => CursorIcon::Grabbing,
            servo::Cursor::EResize => CursorIcon::EResize,
            servo::Cursor::NResize => CursorIcon::NResize,
            servo::Cursor::NeResize => CursorIcon::NeResize,
            servo::Cursor::NwResize => CursorIcon::NwResize,
            servo::Cursor::SResize => CursorIcon::SResize,
            servo::Cursor::SeResize => CursorIcon::SeResize,
            servo::Cursor::SwResize => CursorIcon::SwResize,
            servo::Cursor::WResize => CursorIcon::WResize,
            servo::Cursor::EwResize => CursorIcon::EwResize,
            servo::Cursor::NsResize => CursorIcon::NsResize,
            servo::Cursor::NeswResize => CursorIcon::NeswResize,
            servo::Cursor::NwseResize => CursorIcon::NwseResize,
            servo::Cursor::ColResize => CursorIcon::ColResize,
            servo::Cursor::RowResize => CursorIcon::RowResize,
            servo::Cursor::AllScroll => CursorIcon::AllScroll,
            servo::Cursor::ZoomIn => CursorIcon::ZoomIn,
            servo::Cursor::ZoomOut => CursorIcon::ZoomOut,
            _ => CursorIcon::Default,
        };
        
        // TODO: Only update if this is the hovered pane (Phase 2)
        trace!("Rust: [UI] Cursor changed to {:?} for {} pane {}", winit_cursor, self.window_id, self.pane_id);
        self.window.set_cursor(winit_cursor);
    }
}

// ------------------------------------------------------------------
// WAKER STRATEGY
// ------------------------------------------------------------------

#[derive(Clone)]
struct LotusWaker(EventLoopProxy<EngineCommand>, Arc<AtomicBool>);

impl servo::EventLoopWaker for LotusWaker {
    fn clone_box(&self) -> Box<dyn servo::EventLoopWaker> {
        Box::new(self.clone())
    }
    fn wake(&self) {
        if self.1.swap(true, Ordering::SeqCst) == false {
            let _ = self.0.send_event(EngineCommand::Wake);
        }
    }
}

// ------------------------------------------------------------------
// INTERNAL APP HANDLER (Winit 0.30)
// ------------------------------------------------------------------

static WEBVIEW_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct LotusApp {
    servo: Option<servo::Servo>,
    windows: HashMap<String, WindowInstance>,
    winit_id_to_uuid: HashMap<WindowId, String>,
    proxy: EventLoopProxy<EngineCommand>,
    callback: ThreadsafeFunction<(String, Vec<napi::bindgen_prelude::Buffer>), ErrorStrategy::Fatal>,
    pending_wake: Arc<AtomicBool>,
}

impl Drop for LotusApp {
    fn drop(&mut self) {
        info!("Rust: [Teardown] LotusApp dropping, clearing windows first");
        // Drop all WebViews before shutting down Servo to avoid hangs
        self.windows.clear();
        info!("Rust: [Teardown] Windows cleared, shutting down Servo");
        self.servo = None;
    }
}

impl LotusApp {
    fn new(
        proxy: EventLoopProxy<EngineCommand>,
        callback: ThreadsafeFunction<(String, Vec<napi::bindgen_prelude::Buffer>), ErrorStrategy::Fatal>,
    ) -> Self {
        let mut app = Self {
            servo: None,
            windows: HashMap::new(),
            winit_id_to_uuid: HashMap::new(),
            proxy,
            callback,
            pending_wake: Arc::new(AtomicBool::new(false)),
        };
        app.ensure_servo();
        app
    }
    
    fn ensure_servo(&mut self) -> &servo::Servo {
        if self.servo.is_none() {
            info!("Rust: Initializing Servo Singleton");
            let mut prefs = servo::prefs::Preferences::default();
            prefs.shell_background_color_rgba = [0.0, 0.0, 0.0, 0.0]; // Transparent
            // removed prefs.gfx_precache_shaders = true so that shaders compile lazily (Option A)
            // prefs.gfx_precache_shaders = true;

            let waker = LotusWaker(self.proxy.clone(), self.pending_wake.clone());
            let servo = ServoBuilder::default()
                .event_loop_waker(Box::new(waker))
                .preferences(prefs)
                .build();
            servo.setup_logging();
            self.servo = Some(servo);
        }
        self.servo.as_ref().unwrap()
    }
}

impl ApplicationHandler<EngineCommand> for LotusApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // App is ready to create windows
        info!("Rust: Application Resumed");
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Servo needs to process any queued internal events before the event loop sleeps.
        // This mirrors servoshell's `pump_servo_event_loop` call structure.
        // Without this, Servo's internal work (like deferred layout/paint) doesn't get
        // picked up until an external event wakes the loop.
        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }

        // Consolidated Redraw Request: 
        // If any pane in any window needs a repaint, request one now.
        for window in self.windows.values() {
            let mut any_needs_repaint = false;
            let mut reason = String::new();
            for pane in window.panes.values() {
                // If it needs repaint, we should redraw. 
                // Even if dirty, we want to hit RedrawRequested so we can discard the stale frame and unlock.
                if pane.needs_repaint {
                    any_needs_repaint = true;
                    reason = format!("Pane '{}' needs repaint", pane.id);
                }
            }
            if any_needs_repaint {
                trace!("Rust: [Lifecycle] Requesting Redraw for window (Reason: {})", reason);
                window.window.request_redraw();
            }
        }

        event_loop.set_control_flow(ControlFlow::Wait);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: EngineCommand) {
        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }
        match event {
            EngineCommand::Wake => {
                self.pending_wake.store(false, Ordering::SeqCst);
                // trace!("Rust: Wake received");
            },
            EngineCommand::CreateWindow(options, window_id) => {
                info!("Rust: CreateWindow received for {}", window_id);
                let creation_start = Instant::now();
                
                if let Some(state) = APP_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        s.window_start_times.insert(window_id.clone(), creation_start);
                    }
                }

                
                // Detect Theme explicitly
                #[cfg(target_os = "linux")]
                let mode = detect_linux_theme_robust();
                #[cfg(not(target_os = "linux"))]
                let mode = dark_light::detect();

                info!("Rust: Detected system theme mode: {:?}", mode);
                
                let theme = match mode {
                    dark_light::Mode::Dark => Some(winit::window::Theme::Dark),
                    dark_light::Mode::Light => Some(winit::window::Theme::Light),
                    dark_light::Mode::Default => None,
                };
                
                if let Some(t) = theme {
                     info!("Rust: Setting winit window theme to: {:?}", t);
                } else {
                     info!("Rust: Winit window theme set to None (Default)");
                }

                info!("Rust: Creating window with visible=false, transparent={}", options.transparent);

                let mut window_attrs = Window::default_attributes()
                    .with_title(options.title.clone())
                    .with_inner_size(WinitPhysicalSize::new(
                        options.width,
                        options.height
                    ))
                    .with_decorations(!options.frameless)
                    .with_visible(false) // Always start hidden
                    .with_transparent(options.transparent)
                    .with_theme(theme);

                // NOTE: We intentionally do NOT set no_redirection_bitmap here.
                // WS_EX_NOREDIRECTIONBITMAP requires a DirectComposition visual tree to
                // display content. ANGLE/surfman create a regular DXGI swap chain for the
                // HWND which is NOT compatible with that flag — the window becomes fully
                // transparent/invisible rather than showing the rendered content.

                #[cfg(target_os = "linux")]
                if let Some(class) = &options.wm_class {
                    // Use the class name for both instance and class for simplicity, or split it if needed.
                    // X11
                    // X11
                    window_attrs = WindowAttributesExtX11::with_name(window_attrs, class.clone(), class.clone());
                    // Wayland
                    window_attrs = WindowAttributesExtWayland::with_name(window_attrs, class.clone(), class.clone());
                }

                if options.restore_state {
                    if let Some(state) = APP_STATE.get() {
                        if let Ok(s) = state.lock() {
                            if let Some(ws) = s.window_states.get_window_state(&window_id) {
                                window_attrs = window_attrs
                                    .with_inner_size(WinitPhysicalSize::new(ws.width, ws.height))
                                    .with_position(winit::dpi::PhysicalPosition::new(ws.x, ws.y));
                            }
                        }
                    }
                }

                let window = match event_loop.create_window(window_attrs) {
                    Ok(w) => Arc::new(w),
                    Err(e) => {
                        error!("Failed to create window: {}", e);
                        return;
                    }
                };

                let winit_id = window.id();
                
                let display_handle = match window.display_handle() {
                    Ok(h) => h,
                    Err(e) => {
                        error!("Failed to get display handle: {}", e);
                        return;
                    }
                };
                let window_handle = match window.window_handle() {
                    Ok(h) => h,
                    Err(e) => {
                        error!("Failed to get window handle: {}", e);
                        return;
                    }
                };
                let size = window.inner_size();
                
                let rendering_context = match WindowRenderingContext::new(
                    display_handle,
                    window_handle,
                    size
                ) {
                    Ok(ctx) => Rc::new(ctx),
                    Err(e) => {
                        error!("Failed to create rendering context: {:?}", e);
                        return;
                    }
                };
                
                let _ = rendering_context.make_current();

                let gl = rendering_context.glow_gl_api();

                // Apply Vibrancy/Transparency effects
                if options.transparent {
                    #[cfg(target_os = "macos")]
                    {
                        let _ = apply_vibrancy(&window, NSVisualEffectMaterial::HudWindow, None, None);
                    }
                    
                    #[cfg(target_os = "windows")]
                    {
                        //there might be an issue with the blur on windows through egl/angle so commenting out for now
                        // Try Mica first, fall back to blur
                        // let _ = apply_mica(&window, None)
                        //    .or_else(|_| apply_blur(&window, None));
                    }
                }
                
                let servo = self.ensure_servo().clone();
                
                // Get msgpackr source, port and token from state
                let mut panes = HashMap::new();
                let mut primary_pane_id = "main".to_string();
                
                // 1. Setup shared components
                let hidpi_scale_factor_val = window.scale_factor() as f32;
                let hidpi_scale_factor = Scale::<f32, DeviceIndependentPixel, DevicePixel>::new(hidpi_scale_factor_val);
                
                let (msgpackr_source, port, token) = if let Some(state) = APP_STATE.get() {
                    if let Ok(s) = state.lock() {
                        (s.msgpackr_source.clone(), s.ipc_server_port, s.ipc_server_token.clone())
                    } else { ("".to_string(), 0, "".to_string()) }
                } else { ("".to_string(), 0, "".to_string()) };

                if options.panes.is_empty() {
                    // LEGACY MODE: Create 'main' automatically
                    let main_url_str = options.initial_url.clone().unwrap_or_else(|| "about:blank".to_string());
                    let main_rect = euclid::Rect::new(
                        euclid::Point2D::new(0.0, 0.0),
                        euclid::Size2D::new(size.width as f32 / hidpi_scale_factor_val, size.height as f32 / hidpi_scale_factor_val)
                    );
                    
                    let main_offscreen = Rc::new(rendering_context.offscreen_context(ServoPhysicalSize::new(size.width, size.height)));
                    let ready_to_repaint = Arc::new(AtomicBool::new(true));
                    let main_delegate = Rc::new(LotusPaneDelegate {
                        window: window.clone(),
                        window_id: window_id.clone(),
                        pane_id: "main".to_string(),
                        proxy: self.proxy.clone(),
                        ready_to_repaint: ready_to_repaint.clone(),
                    });
                    
                    let main_ucm = Rc::new(UserContentManager::new(&servo));
                    main_ucm.add_script(Rc::new(UserScript::from(msgpackr_source.as_str())));
                    main_ucm.add_script(Rc::new(UserScript::from(IPC_BOOTSTRAP_BASE)));
                    let main_port_script = format!("window.lotus.port = {}; window.lotus.token = '{}'; window.lotus.id = '{}'; window.lotus.paneId = 'main';", port, token, window_id);
                    main_ucm.add_script(Rc::new(UserScript::from(main_port_script.as_str())));
                    
                    let theme_str = match mode { dark_light::Mode::Dark => "dark", _ => "light" };
                    let theme_script = format!(r#"
                        window.lotus.theme = '{}';
                        try {{ document.documentElement.dataset.theme = window.lotus.theme; }} catch(e) {{}}
                    "#, theme_str);
                    main_ucm.add_script(Rc::new(UserScript::from(theme_script.as_str())));
                    main_ucm.add_script(Rc::new(UserScript::from(DRAG_REGION_SCRIPT)));
                    
                    let mut main_builder = WebViewBuilder::new(&servo, main_offscreen.clone())
                        .delegate(main_delegate)
                        .hidpi_scale_factor(hidpi_scale_factor)
                        .user_content_manager(main_ucm);
                    
                    if let Ok(u) = url::Url::parse(&main_url_str) {
                        main_builder = main_builder.url(u);
                    }

                    let main_initial_size = ServoPhysicalSize::new(size.width, size.height);
                    let main_pane = PaneInstance {
                        id: "main".to_string(),
                        webview: main_builder.build(),
                        rect: main_rect,
                        last_notified_rect: None,
                        last_physical_rect: euclid::default::Rect::new(
                            euclid::default::Point2D::new(0, 0),
                            euclid::default::Size2D::new(size.width as i32, size.height as i32)
                        ),
                        z_index: 0,
                        anchor: if options.auto_resize_main { PaneAnchor::Fill } else { PaneAnchor::None },
                        dock_order: 0,
                        animating: false,
                        is_dirty: true,
                        needs_repaint: false,
                        is_visible: false,
                        drag_regions: Vec::new(),

                        no_drag_regions: Vec::new(),
                        offscreen_ctx: main_offscreen,
                        servo_busy: false,
                        pending_servo_size: None,
                        requested_servo_size: main_initial_size,
                        current_servo_size: main_initial_size,
                        ready_to_repaint,
                        ready_frame_size: None,
                        first_frame_painted: false,
                        first_frame_painted_time: None,
                        pending_physical_rect: None,
                        is_resizing: false,
                        pending_visible: true,
                        frames_until_stable: 0,
                        ghost_tex: None,
                        ghost_tex_size: None,
                    };
                    panes.insert("main".to_string(), main_pane);
                    primary_pane_id = "main".to_string();
                } else {
                    // PRO MODE: Use absolute source of truth from panes array
                    for (i, pane_opt) in options.panes.iter().enumerate() {
                        if i == 0 { primary_pane_id = pane_opt.id.clone(); }
                        
                        let p_rect = euclid::Rect::new(
                            euclid::Point2D::new(pane_opt.x as f32, pane_opt.y as f32),
                            euclid::Size2D::new(pane_opt.width as f32, pane_opt.height as f32)
                        );
                        
                        let p_physical_w = (p_rect.width() * hidpi_scale_factor_val).round() as u32;
                        let p_physical_h = (p_rect.height() * hidpi_scale_factor_val).round() as u32;
                        // OpenGL requires at least 1x1 for framebuffer completeness
                        let p_physical_w = p_physical_w.max(1);
                        let p_physical_h = p_physical_h.max(1);

                        let p_offscreen = Rc::new(rendering_context.offscreen_context(ServoPhysicalSize::new(p_physical_w, p_physical_h)));
                        let p_ready_to_repaint = Arc::new(AtomicBool::new(true));
                        let p_delegate = Rc::new(LotusPaneDelegate {
                            window: window.clone(),
                            window_id: window_id.clone(),
                            pane_id: pane_opt.id.clone(),
                            proxy: self.proxy.clone(),
                            ready_to_repaint: p_ready_to_repaint.clone(),
                        });
                        
                        let p_ucm = Rc::new(UserContentManager::new(&servo));
                        p_ucm.add_script(Rc::new(UserScript::from(msgpackr_source.as_str())));
                        p_ucm.add_script(Rc::new(UserScript::from(IPC_BOOTSTRAP_BASE)));
                        let p_port_script = format!("window.lotus.port = {}; window.lotus.token = '{}'; window.lotus.id = '{}'; window.lotus.paneId = '{}';", port, token, window_id, pane_opt.id);
                        p_ucm.add_script(Rc::new(UserScript::from(p_port_script.as_str())));
                        
                        let theme_str = match mode { dark_light::Mode::Dark => "dark", _ => "light" };
                        let theme_script = format!(r#"
                            window.lotus.theme = '{}';
                            try {{ document.documentElement.dataset.theme = window.lotus.theme; }} catch(e) {{}}
                        "#, theme_str);
                        p_ucm.add_script(Rc::new(UserScript::from(theme_script.as_str())));
                        p_ucm.add_script(Rc::new(UserScript::from(DRAG_REGION_SCRIPT)));
                        
                        let mut p_builder = WebViewBuilder::new(&servo, p_offscreen.clone())
                            .delegate(p_delegate)
                            .hidpi_scale_factor(hidpi_scale_factor)
                            .user_content_manager(p_ucm);
                        
                        if let Ok(u) = url::Url::parse(&pane_opt.url) {
                            p_builder = p_builder.url(u);
                        }

                        let p_initial_size = ServoPhysicalSize::new(p_physical_w, p_physical_h);
                        let pane = PaneInstance {
                            id: pane_opt.id.clone(),
                            webview: p_builder.build(),
                            rect: p_rect,
                            last_notified_rect: None,
                            last_physical_rect: euclid::default::Rect::new(
                                euclid::default::Point2D::new(0, 0),
                                euclid::default::Size2D::new(p_physical_w as i32, p_physical_h as i32)
                            ),
                            z_index: pane_opt.z_index,
                            anchor: PaneAnchor::from(pane_opt.anchor.unwrap_or(0)),
                            dock_order: pane_opt.dock_order.unwrap_or(0),
                            animating: false,
                            is_dirty: true,
                            needs_repaint: false,
                            is_visible: false,
                            drag_regions: Vec::new(),
                            no_drag_regions: Vec::new(),
                            offscreen_ctx: p_offscreen,
                            servo_busy: false,
                            pending_servo_size: None,
                            requested_servo_size: p_initial_size,
                            current_servo_size: p_initial_size,
                            ready_to_repaint: p_ready_to_repaint,
                            ready_frame_size: None,
                            first_frame_painted: false,
                        first_frame_painted_time: None,
                            pending_physical_rect: None,
                            is_resizing: false,
                            pending_visible: pane_opt.visible,
                            frames_until_stable: 0,
                        ghost_tex: None,
                        ghost_tex_size: None,
                            };
                        panes.insert(pane_opt.id.clone(), pane);
                    }
                }

                let mut id_shifters = Vec::new();
                let shifter = WebViewBuilder::new(&servo, rendering_context.clone()).build();
                let shifter_count = WEBVIEW_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                info!("Rust: Created Shifter WebView #{}", shifter_count);
                id_shifters.push(shifter);

                let initial_layout = StagedLayout {
                    panes: HashMap::new(),
                    width: size.width,
                    height: size.height,
                    scale_factor: hidpi_scale_factor_val,
                };

                let mut instance = WindowInstance {
                    panes,
                    active_layout: initial_layout.clone(),
                    staged_layout: initial_layout,
                    active_pane_id: primary_pane_id.clone(),
                    primary_pane_id,
                    last_mouse_down_pane_id: None,
                    rendering_context,
                    gl,
                    window: window.clone(),
                    last_mouse_pos: Point2D::new(0.0, 0.0),
                    is_mouse_down: false,
                    modifiers: Modifiers::default(),
                    frameless: options.frameless,
                    transparent: options.transparent,
                    in_resize_border: false,
                    auto_resize_main: options.auto_resize_main,
                    corner_radius: options.corner_radius,
                    id_shifters,
                    emitted_ready_to_show: false,
                    pending_stabilization: false,
                    stencil_program: None,
                    stencil_vao: None,
                    u_stencil_size_loc: None,
                    u_stencil_radius_loc: None,
                    comp_program: None,
                    comp_fbo: None,
                    comp_tex: None,
                    comp_vao: None,
                    comp_vbo: None,
                    u_comp_size_loc: None,
                    u_comp_radius_loc: None,
                    comp_tex_size: None,
                    scene_fbo: None,
                    scene_tex: None,
                    scene_tex_size: None,
                    committed_layout: None,
                    active_window_size: winit::dpi::PhysicalSize::new(size.width, size.height),
                    committed_window_size: None,
                };

                instance.init_stencil_program();
                instance.init_composition_resources();

                instance.recalculate_layout(winit::dpi::PhysicalSize::new(size.width, size.height), hidpi_scale_factor_val);

                self.windows.insert(window_id.clone(), instance);
                self.winit_id_to_uuid.insert(winit_id, window_id.clone());

                if let Some(state) = APP_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        s.window_metadata.insert(window_id.clone(), WindowMetadata {
                            root_path: options.root.clone().map(PathBuf::from),
                            last_window_size: Some(size),
                        });

                    }
                }

                if options.visible {
                    window.set_visible(true);
                }
                window.request_redraw();

                if let Some(state) = APP_STATE.get() {
                    if let Ok(s) = state.lock() {
                        if s.profiling {
                            eprintln!("[PROFILE] Window {} ready in {:?}", window_id, creation_start.elapsed());
                        }
                    }
                }

                let actual_size = window.inner_size();
                let actual_scale = window.scale_factor() as f32;
                if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                    "event": "_internal-created",
                    "window_id": window_id,
                    "width": actual_size.width,
                    "height": actual_size.height,
                    "scale_factor": actual_scale,
                    "logicalWidth": actual_size.width as f32 / actual_scale,
                    "logicalHeight": actual_size.height as f32 / actual_scale
                })) {
                    let mut wrapped = Vec::with_capacity(msg.len() + 1);
                    wrapped.push(MSG_TYPE_DATA);
                    wrapped.extend(msg);
                    self.callback.call((window_id.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                }

                info!("Window created successfully: {}", window_id);
            },
            EngineCommand::Quit => {
                info!("Rust: [Teardown] Quit command received. Starting 3s safety watchdog.");
                // Start a watchdog immediately in case the event loop hangs during teardown
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(3000));
                    eprintln!("Rust: [Teardown] Watchdog timeout. Forcing exit.");
                    std::process::exit(0);
                });
                event_loop.exit();
            },
            EngineCommand::IpcMessage(window_id, raw_bytes) => {
                // For singular messages (mostly from internal Rust sources),
                // we ensure the MSG_TYPE_DATA header is present so JS doesn't have to guess.
                let (msg_type, payload) = if !raw_bytes.is_empty() {
                    (raw_bytes[0], &raw_bytes[1..])
                } else {
                    (MSG_TYPE_DATA, &raw_bytes[..])
                };

                if msg_type == MSG_TYPE_CONTROL {
                    intercept_drag_regions(payload, window_id.clone());
                }

                if !raw_bytes.is_empty() && (raw_bytes[0] == MSG_TYPE_CONTROL || raw_bytes[0] == MSG_TYPE_DATA || raw_bytes[0] == 0x03) {
                    self.callback.call((window_id, vec![raw_bytes.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                } else {
                    let mut wrapped = Vec::with_capacity(raw_bytes.len() + 1);
                    wrapped.push(MSG_TYPE_DATA);
                    wrapped.extend(raw_bytes);
                    self.callback.call((window_id, vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                }
            },
            EngineCommand::IpcMessages(window_id, messages) => {
                let mut processed_messages = Vec::with_capacity(messages.len());
                for raw_bytes in messages {
                    // Pre-process messages to intercept known internal Lotus commands.
                    // The 1-byte header allows us to skip O(N) scanning for almost all messages.
                    let (msg_type, payload) = if raw_bytes.len() > 0 {
                        (raw_bytes[0], &raw_bytes[1..])
                    } else {
                        (MSG_TYPE_DATA, &raw_bytes[..])
                    };

                    info!("Rust: Processing IPC message from {}. Type: 0x{:02x}, Payload len: {}", window_id, msg_type, payload.len());

                    if msg_type == MSG_TYPE_CONTROL {
                        intercept_drag_regions(payload, window_id.clone());
                    }

                    // Forward the raw bytes (including the 1-byte header) to Node.js.
                    // This allows JS to handle both standard and chunked (0x03) messages.
                    processed_messages.push(raw_bytes.into());
                }
                
                if !processed_messages.is_empty() {
                    self.callback.call((window_id, processed_messages), ThreadsafeFunctionCallMode::NonBlocking);
                }
            },
            EngineCommand::LoadUrl(window_id, pane_id, url) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    let pid = if pane_id == "main" && !instance.panes.contains_key("main") {
                        &instance.primary_pane_id
                    } else {
                        &pane_id
                    };
                    
                    if let Some(pane) = instance.panes.get(pid) {
                        if let Ok(u) = url::Url::parse(&url) {
                            pane.webview.load(u);
                        }
                    }
                }
            },
            EngineCommand::Resize(window_id, size) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    // auto_resize_main is now handled entirely in JS via the throttled 'resize' event.
                    // This ensures the main pane benefits from the same 25ms resize throttle
                    // and compositor stretching as manual multi-pane configurations.
                    instance.rendering_context.resize(size);
                }
            },
            EngineCommand::SetPosition(window_id, position) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_outer_position(position);
                }
            },
            EngineCommand::SetAlwaysOnTop(window_id, flag) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    platform::set_always_on_top(&instance.window, flag);
                }
            },
            EngineCommand::RequestAttention(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    platform::request_attention(&instance.window);
                }
            },
            EngineCommand::SetTitle(window_id, title) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_title(&title);
                }
            },
            EngineCommand::CloseWindow(window_id) => {
                self.windows.remove(&window_id);
                // Clean up per-window metadata so long-running apps don't leak.
                if let Some(state) = APP_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        s.window_metadata.remove(&window_id);
                        s.window_start_times.remove(&window_id);
                    }
                }
                // Drop any buffered outgoing frames for this window.
                if let Some(p) = WS_PENDING.get() { p.remove(&window_id); }
                info!("Closed window: {}", window_id);
            },
            EngineCommand::SetDecorations(window_id, decorations) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_decorations(decorations);
                }
            },
            EngineCommand::ExecuteScript(window_id, pane_id, script) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    let pid = if pane_id == "main" && !instance.panes.contains_key("main") {
                        instance.primary_pane_id.clone()
                    } else {
                        pane_id.clone()
                    };

                    if let Some(pane) = instance.panes.get(&pid) {
                        info!("Rust: [IPC] Executing script in window '{}' pane '{}' (len: {})", window_id, pid, script.len());
                        pane.webview.evaluate_javascript(&script, |_| {});
                    } else {
                        warn!("Rust: ExecuteScript failed - pane '{}' not found in window '{}'", pid, window_id);
                    }
                }
            },            EngineCommand::ShowWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_visible(true);
                }
            },
            EngineCommand::HideWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_visible(false);
                }
            },
            EngineCommand::UpdateDragRegions(window_id, pane_id, drag_regions, no_drag_regions) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        pane.drag_regions = drag_regions;
                        pane.no_drag_regions = no_drag_regions;
                    }
                }
            },
            EngineCommand::MinimizeWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_minimized(true);
                }
            },
            EngineCommand::UnminimizeWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_minimized(false);
                }
            },
            EngineCommand::MaximizeWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_maximized(true);
                }
            },
            EngineCommand::UnmaximizeWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_maximized(false);
                }
            },
            EngineCommand::FocusWindow(window_id) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.focus_window();
                }
            },
            EngineCommand::AnimatingChanged(window_id, pane_id, animating) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        pane.animating = animating;
                        trace!("Rust: Window {} Pane {} animating={}", window_id, pane_id, animating);
                    }
                }
            },
            EngineCommand::NewFrameReady(window_id, pane_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        trace!("Rust: [Lifecycle] EngineCommand::NewFrameReady for window {} pane {}", window_id, pane_id);
                        pane.servo_busy = false;
                        
                        // Unlock BEFORE painting to allow Servo to begin preparing the NEXT frame
                        pane.ready_to_repaint.store(true, Ordering::SeqCst);
                        
                        // THE FIX: Unconditionally accept the frame Servo just spent time rendering!
                        // This allows the current committed_layout transaction to clear, bringing 
                        // the UI up to date with this frame before starting the next one.
                        pane.current_servo_size = pane.requested_servo_size;
                        pane.ready_frame_size = Some(pane.current_servo_size);
                        pane.needs_repaint = true;
                        
                        let was_first_frame = !pane.first_frame_painted;
                        if was_first_frame {
                            pane.first_frame_painted_time = Some(std::time::Instant::now());
                        }
                        pane.first_frame_painted = true;
                        
                        // ONE-FRAME LATCH: Signal that we need one full paint cycle
                        if pane.pending_physical_rect.is_some() {
                            pane.frames_until_stable = 1;
                        }
                        
                        pane.is_dirty = false;
                        let is_locked = instance.committed_layout.is_some();
                        
                        // THE FIX: If locked, immediately dispatch the pending transaction target to Servo.
                        // If unlocked, dispatch any queued resizes (continuous dragging).
                        if is_locked {
                            if let Some(committed) = &instance.committed_layout {
                                if let Some(staged) = committed.panes.get(&pane.id) {
                                    let target_size = ServoPhysicalSize::new(
                                        (staged.rect.size.width as u32).max(1),
                                        (staged.rect.size.height as u32).max(1)
                                    );
                                    if pane.current_servo_size != target_size {
                                        pane.webview.resize(target_size);
                                        pane.requested_servo_size = target_size;
                                        pane.servo_busy = true;
                                        pane.pending_servo_size = None;
                                    }
                                }
                            }
                        } else {
                            if let Some(pending) = pane.pending_servo_size.take() {
                                pane.webview.resize(pending);
                                pane.requested_servo_size = pending;
                                pane.servo_busy = true;
                                pane.is_dirty = true;
                            }
                        }

                        // AGGREGATION FIX: Only emit when all visible panes have painted
                        if !instance.emitted_ready_to_show {
                                let all_ready = instance.panes.values().all(|p| !p.is_visible || p.first_frame_painted);
                                
                                if all_ready {
                                    instance.emitted_ready_to_show = true;
                                    
                                    if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                                        "event": "ready-to-show",
                                        "window_id": window_id,
                                        "pane_id": pane_id
                                    })) {
                                        let mut wrapped = Vec::with_capacity(msg.len() + 1);
                                        wrapped.push(MSG_TYPE_DATA);
                                        wrapped.extend(msg);
                                        self.callback.call((window_id.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                                    }
                                }
                            }
                        instance.window.request_redraw();
                    }
                }
            },
            EngineCommand::SetMinInnerSize(window_id, size) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_min_inner_size(size);
                }
            },
            EngineCommand::SetMaxInnerSize(window_id, size) => {
                if let Some(instance) = self.windows.get(&window_id) {
                    instance.window.set_max_inner_size(size);
                }
            },
            EngineCommand::CreatePane(window_id, pane_id, url, rect, z_index, anchor, dock_order) => {
                let servo = self.ensure_servo().clone();
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if pane_id == "main" {
                        instance.auto_resize_main = false;
                    }

                    let ready_to_repaint = Arc::new(AtomicBool::new(true));
                    let delegate = Rc::new(LotusPaneDelegate {
                        window: instance.window.clone(),
                        window_id: window_id.clone(),
                        pane_id: pane_id.clone(),
                        proxy: self.proxy.clone(),
                        ready_to_repaint: ready_to_repaint.clone(),
                    });                    
                    let hidpi_scale_factor_val = instance.window.scale_factor() as f32;
                    let hidpi_scale_factor = Scale::<f32, DeviceIndependentPixel, DevicePixel>::new(hidpi_scale_factor_val);
                    
                    let user_content_manager = Rc::new(UserContentManager::new(&servo));
                    
                    let (msgpackr_source, port, token) = if let Some(state) = APP_STATE.get() {
                        if let Ok(s) = state.lock() {
                            (s.msgpackr_source.clone(), s.ipc_server_port, s.ipc_server_token.clone())
                        } else {
                            ("".to_string(), 0, "".to_string())
                        }
                    } else {
                        ("".to_string(), 0, "".to_string())
                    };

                    user_content_manager.add_script(Rc::new(UserScript::from(msgpackr_source.as_str())));
                    user_content_manager.add_script(Rc::new(UserScript::from(IPC_BOOTSTRAP_BASE)));

                    let port_script = format!("window.lotus.port = {}; window.lotus.token = '{}'; window.lotus.id = '{}'; window.lotus.paneId = '{}';", port, token, window_id, pane_id);
                    user_content_manager.add_script(Rc::new(UserScript::from(port_script.as_str())));
                    user_content_manager.add_script(Rc::new(UserScript::from(DRAG_REGION_SCRIPT)));

                    let servo_size = ServoPhysicalSize::new(
                        (rect.size.width * hidpi_scale_factor_val).round() as u32,
                        (rect.size.height * hidpi_scale_factor_val).round() as u32
                    );
                    // OpenGL requires at least 1x1 for framebuffer completeness
                    let servo_size = ServoPhysicalSize::new(
                        servo_size.width.max(1),
                        servo_size.height.max(1)
                    );
                    let offscreen_ctx = Rc::new(instance.rendering_context.offscreen_context(servo_size));

                    let mut webview_builder = WebViewBuilder::new(&servo, offscreen_ctx.clone())
                        .delegate(delegate)
                        .hidpi_scale_factor(hidpi_scale_factor)
                        .user_content_manager(user_content_manager);

                    // Force unique ID by keeping a shifter alive
                    let shifter = WebViewBuilder::new(&servo, instance.rendering_context.clone()).build();
                    let shifter_count = WEBVIEW_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    info!("Rust: Created Shifter WebView #{}", shifter_count);
                    instance.id_shifters.push(shifter);

                    if let Ok(u) = url::Url::parse(&url) {
                        webview_builder = webview_builder.url(u);
                    }

                    let webview = webview_builder.build();
                    let count = WEBVIEW_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    info!("Rust: Created additional WebView #{} for pane '{}'", count, pane_id);
                    
                    let pane = PaneInstance {
                        id: pane_id.clone(),
                        webview,
                        rect,
                        last_notified_rect: None,
                        last_physical_rect: euclid::default::Rect::new(
                            euclid::default::Point2D::new(0, 0),
                            euclid::default::Size2D::new(servo_size.width as i32, servo_size.height as i32)
                        ),
                        z_index,
                        anchor,
                        dock_order,
                        animating: false,
                        is_dirty: true,
                        needs_repaint: false,
                        is_visible: false,
                        drag_regions: Vec::new(),

                        no_drag_regions: Vec::new(),
                        offscreen_ctx,
                        servo_busy: false,
                        pending_servo_size: None,
                        requested_servo_size: servo_size,
                        current_servo_size: servo_size,
                        ready_to_repaint,
                        ready_frame_size: None,
                        first_frame_painted: false,
                        first_frame_painted_time: None,
                        pending_physical_rect: None,
                        is_resizing: false,
                        pending_visible: true,
                        frames_until_stable: 0,
                        ghost_tex: None,
                        ghost_tex_size: None,
                    };
                    instance.panes.insert(pane_id, pane);
                }
            },
            EngineCommand::RemovePane(window_id, pane_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    let _ = instance.panes.remove(&pane_id);
                    // Focus fallback: If the removed pane was active, move focus to primary.
                    if instance.active_pane_id == pane_id {
                        instance.active_pane_id = instance.primary_pane_id.clone();
                    }
                }
            },
            EngineCommand::SetPaneRect(window_id, pane_id, rect) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if pane_id == "main" {
                        instance.auto_resize_main = false;
                    }
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        let scale_factor = instance.window.scale_factor() as f32;
                        let size_changed = pane.last_notified_rect.map(|r| r.size != rect.size).unwrap_or(true);
                        
                        if !size_changed && !pane.is_dirty && pane.last_notified_rect == Some(rect) {
                            return;
                        }
                        
                        pane.rect = rect;
                        pane.last_notified_rect = Some(rect);
                        
                        let pane_phys = euclid::default::Rect::new(
                            euclid::default::Point2D::new((rect.origin.x * scale_factor).round() as i32, (rect.origin.y * scale_factor).round() as i32),
                            euclid::default::Size2D::new((rect.size.width * scale_factor).round() as i32, (rect.size.height * scale_factor).round() as i32)
                        );
                        pane.pending_physical_rect = Some(pane_phys);
                        pane.anchor = PaneAnchor::None;

                        let servo_size = ServoPhysicalSize::new(
                            ((rect.size.width * scale_factor).round() as u32).max(1),
                            ((rect.size.height * scale_factor).round() as u32).max(1)
                        );
                        
                        debug!("Rust: SetPaneRect for '{}' -> {:?} (Physical: {}x{})", pane_id, rect, servo_size.width, servo_size.height);
                        
                        if size_changed || pane.is_dirty {
                            pane.is_resizing = true;
                            if !size_changed {
                                // Position shift only: Join the Nuclear Flip but don't hold it back
                                pane.frames_until_stable = 0;
                            } else {
                                // Physical resize: Hold the Nuclear Flip until Servo responds
                                pane.pending_servo_size = Some(servo_size);
                            }
                        } else {
                            // No change needed, but ensure we don't leave it in resizing state
                            pane.is_resizing = false;
                            pane.pending_physical_rect = None;
                        }
                        
                        let physical_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;
                        instance.recalculate_layout(physical_size, scale_factor);
                        instance.window.request_redraw();
                    }
                }
            },
            EngineCommand::SetPaneVisible(window_id, pane_id, visible) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(pane) = instance.panes.get_mut(&pane_id) {
                        pane.pending_visible = visible;
                        pane.is_dirty = true; // Force reflow
                        
                        let physical_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;
                        instance.recalculate_layout(physical_size, scale_factor);
                        instance.window.request_redraw();
                    }
                }
            },
            EngineCommand::FocusPane(window_id, pane_id) => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    instance.active_pane_id = pane_id;
                }
            },
        }
        
        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }
        event_loop.set_control_flow(ControlFlow::Wait);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        // Log every RedrawRequested BEFORE any guard, so we know if winit is dispatching it at all
        if matches!(event, WindowEvent::RedrawRequested) {
            // info!("Rust: [RAW] RedrawRequested fired for winit id {:?}, known windows: {}", 
            //    window_id, self.winit_id_to_uuid.len());
        }

        match event {
            WindowEvent::ThemeChanged(theme) => {
                if let Some(uuid) = self.winit_id_to_uuid.get(&window_id) {
                     let theme_str = match theme {
                        winit::window::Theme::Dark => "dark",
                        winit::window::Theme::Light => "light",
                    };
                    info!("Theme changed to {} for window {}", theme_str, uuid);
                    
                    if let Some(instance) = self.windows.get(uuid) {
                         let script = format!(r#"
                            if (window.lotus) {{
                                window.lotus.theme = '{}';
                                window.lotus.emit('theme-changed', '{}');
                                try {{ document.documentElement.dataset.theme = '{}'; }} catch(e) {{}}
                            }}
                        "#, theme_str, theme_str, theme_str);
                         for pane in instance.panes.values() {
                            pane.webview.evaluate_javascript(&script, |_| {});
                         }
                    }
                }
            },
            _ => {}
        }

        if let Some(servo) = &self.servo {
            servo.spin_event_loop();
        }
        event_loop.set_control_flow(ControlFlow::Wait);

        if let Some(uuid) = self.winit_id_to_uuid.get(&window_id).cloned() {
            if let Some(instance) = self.windows.get_mut(&uuid) {
                match event {
                    WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                        info!("Rust: Scale factor changed to {}", scale_factor);
                        // Resize every webview to the new physical dimensions so Servo renders
                        // at the correct DPI.  Phase 1 of RedrawRequested will sync the FBOs
                        // but no longer calls webview.resize() itself, so we must do it here.
                        let sf = scale_factor as f32;
                        for pane in instance.panes.values_mut() {
                            let new_pw = ((pane.rect.size.width  * sf).round() as u32).max(1);
                            let new_ph = ((pane.rect.size.height * sf).round() as u32).max(1);
                            if true { // Always valid now since we clamped to >= 1
                                let new_size = ServoPhysicalSize::new(new_pw, new_ph);
                                pane.webview.resize(new_size);
                                pane.current_servo_size = new_size;
                                pane.servo_busy = true;
                                pane.pending_servo_size = None;
                                pane.is_dirty = true;
                                pane.needs_repaint = false;
                            }
                        }
                        instance.window.request_redraw();
                    },
                    WindowEvent::CloseRequested => {
                        info!("Rust: Window close requested");
                        
                        // Save window state before closing
                        if let Some(state) = APP_STATE.get() {
                            if let Ok(mut s) = state.lock() {
                                let position = instance.window.outer_position().ok();
                                let size = instance.window.inner_size();
                                let window_state = window_state::WindowState {
                                    x: position.map(|p| p.x).unwrap_or(0),
                                    y: position.map(|p| p.y).unwrap_or(0),
                                    width: size.width,
                                    height: size.height,
                                    maximized: instance.window.is_maximized(),
                                    fullscreen: false, // Default for now
                                };
                                s.window_states.save_window_state(&uuid, window_state);
                            }
                        }
                        let mut close_msg = Vec::new();
                        if rmp_serde::encode::write(&mut close_msg, &serde_json::json!({"event": "window-closed", "window_id": uuid})).is_ok() {
                            let mut wrapped = Vec::with_capacity(close_msg.len() + 1);
                            wrapped.push(MSG_TYPE_DATA);
                            wrapped.extend(close_msg);
                            self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                        self.windows.remove(&uuid);
                        self.winit_id_to_uuid.remove(&window_id);
                        info!("Rust: Window '{}' closed. {} windows remaining", uuid, self.windows.len());

                        // Clean up per-window metadata to avoid unbounded growth
                        // across many open/close cycles.
                        if let Some(state) = APP_STATE.get() {
                            if let Ok(mut s) = state.lock() {
                                s.window_metadata.remove(&uuid);
                                s.window_start_times.remove(&uuid);
                            }
                        }
                        // Drop any buffered outgoing frames for this window.
                        if let Some(p) = WS_PENDING.get() { p.remove(&uuid); }
                        if self.windows.is_empty() {
                            info!("Rust: Last window closed, exiting event loop");
                            event_loop.exit();
                        }
                    },
                    WindowEvent::RedrawRequested => {
                        let _ = instance.rendering_context.make_current();
                        let window_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;

                        trace!("Rust: [Lifecycle] RedrawRequested (FBO) for window size {}x{}", window_size.width, window_size.height);

                        unsafe {
                            // 1. Evaluate Integrity against the COMMITTED target
                            let is_transaction_pending = instance.committed_layout.is_some();
                            let mut transaction_complete = false;

                            if let Some(committed) = &instance.committed_layout {
                                transaction_complete = true;
                                for (id, staged_pane) in &committed.panes {
                                    if staged_pane.visible {
                                        if let Some(pane) = instance.panes.get(id) {
                                            if pane.is_resizing {
                                                let target_size = ServoPhysicalSize::new(
                                                    (staged_pane.rect.size.width as u32).max(1), 
                                                    (staged_pane.rect.size.height as u32).max(1)
                                                );
                                                let current_ready = pane.ready_frame_size.unwrap_or(ServoPhysicalSize::new(0, 0));
                                                
                                                // THE FIX: Hold the Nuclear Flip back if the sizes don't match OR 
                                                // if the pane needs one background paint cycle to stabilize the FBO
                                                if current_ready.width != target_size.width || current_ready.height != target_size.height || pane.frames_until_stable > 0 {
                                                    transaction_complete = false;
                                                    break;
                                                }
                                                // THE FIX: For the very first spawn of a pane, hold the lock for 150ms 
                                                // to allow WebRender async texture and display list generation to finish.
                                                if let Some(first_time) = pane.first_frame_painted_time {
                                                    if first_time.elapsed() < std::time::Duration::from_millis(150) {
                                                        transaction_complete = false;
                                                        instance.window.request_redraw(); // Keep pumping the event loop
                                                        break;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            let mut _flip_occurred = false;

                            // 2. THE NUCLEAR GEOMETRY FLIP
                            if is_transaction_pending && transaction_complete {
                                if let Some(committed) = instance.committed_layout.take() {
                                    instance.active_layout = committed;
                                    if let Some(c_size) = instance.committed_window_size.take() {
                                        instance.active_window_size = c_size;
                                    }
                                    
                                    for (id, staged) in &instance.active_layout.panes {
                                        if let Some(p) = instance.panes.get_mut(id) {
                                            p.is_visible = staged.visible;
                                            p.last_physical_rect = staged.rect;
                                            p.is_resizing = false;
                                            p.pending_physical_rect = None;
                                            p.first_frame_painted_time = None;
                                        }
                                    }
                                    trace!("Rust: [Transaction] Nuclear Layout Swap Executed");
                                    _flip_occurred = true;
                                    
                                    // CRITICAL RE-ENTRY: Pick up any drag/resize events
                                    instance.recalculate_layout(window_size, scale_factor);

                                    // THE FIX: Reset transaction_complete. If recalculate_layout just established
                                    // a NEW transaction, we must ensure is_locked evaluates to true for the rest
                                    // of this frame to prevent a 1-frame transparency flash.
                                    transaction_complete = false;
                                }
                            }

                            let is_transaction_pending = instance.committed_layout.is_some();
                            let mut pending_hides = std::collections::HashSet::new();
                            if let Some(committed) = &instance.committed_layout {
                                for (id, staged) in &committed.panes {
                                    if !staged.visible {
                                        pending_hides.insert(id.clone());
                                    }
                                }
                            }

                            let mut sorted_panes: Vec<_> = instance.panes.values_mut().collect();
                            sorted_panes.sort_by_key(|p| p.z_index);

                            // 3. UNIVERSAL PAINT BLOCK
                            for p in sorted_panes.iter_mut() {
                                if p.needs_repaint {
                                    // THE FIX: Do not use .take() here! Idle panes must retain their 
                                    // ready_frame_size so subsequent layout transactions know they are stable.
                                    if let Some(target_size) = p.ready_frame_size {
                                        let safe_size = ServoPhysicalSize::new(target_size.width.max(1), target_size.height.max(1));
                                        if p.offscreen_ctx.size() != safe_size {
                                            let _ = p.offscreen_ctx.make_current();
                                            p.offscreen_ctx.resize(safe_size);
                                        }
                                    }

                                    let current_size = p.offscreen_ctx.size();
                                    if current_size.width > 0 && current_size.height > 0 {
                                        let _ = p.offscreen_ctx.make_current();
                                        use glow::HasContext;
                                        while instance.gl.get_error() != glow::NO_ERROR {}
                                        p.webview.paint();
                                        p.first_frame_painted = true;

                                        // THE FIX: Decrement the latch and request the final atomic layout flip
                                        if p.frames_until_stable > 0 {
                                            p.frames_until_stable -= 1;
                                            if p.frames_until_stable == 0 {
                                                instance.window.request_redraw();
                                            }
                                        }
                                    }
                                    p.needs_repaint = false;
                                }
                            }

                            // ---------------------------------------------------------
                            // FBO ALLOCATION (Strictly bound to active_window_size)
                            // ---------------------------------------------------------
                            let active_w = instance.active_window_size.width.max(1);
                            let active_h = instance.active_window_size.height.max(1);
                            let current_safe_size = winit::dpi::PhysicalSize::new(active_w, active_h);
                            
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

                            // ---------------------------------------------------------
                            // PASS 1: COMPOSITE PANES TO OFFSCREEN SCENE FBO
                            // ---------------------------------------------------------
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

                            if let (Some(radius), Some(program)) = (instance.corner_radius, instance.stencil_program) {
                                if radius > 0.0 {
                                    instance.gl.enable(glow::STENCIL_TEST);
                                    instance.gl.stencil_func(glow::ALWAYS, 1, 0xFF);
                                    instance.gl.stencil_op(glow::REPLACE, glow::REPLACE, glow::REPLACE);
                                    instance.gl.color_mask(false, false, false, false);

                                    instance.gl.use_program(Some(program));
                                    instance.gl.bind_vertex_array(instance.stencil_vao);
                                    instance.gl.uniform_2_f32(instance.u_stencil_size_loc.as_ref(), active_w as f32, active_h as f32);
                                    instance.gl.uniform_1_f32(instance.u_stencil_radius_loc.as_ref(), radius as f32 * scale_factor);

                                    instance.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                    
                                    instance.gl.bind_vertex_array(None);
                                    instance.gl.use_program(None);

                                    instance.gl.color_mask(true, true, true, true);
                                    instance.gl.stencil_func(glow::EQUAL, 1, 0xFF);
                                    instance.gl.stencil_op(glow::KEEP, glow::KEEP, glow::KEEP);
                                }
                            }

                            for pane in sorted_panes.iter_mut() {
                                if !pane.is_visible { continue; } 
                                
                                let (layout_pw, layout_ph, _layout_px, _layout_py, tex_pw, tex_ph, tex_px, tex_py) = {
                                    let x_start = pane.last_physical_rect.origin.x;
                                    let y_start = pane.last_physical_rect.origin.y;
                                    let layout_pw = pane.last_physical_rect.size.width;
                                    let layout_ph = pane.last_physical_rect.size.height;
                                    
                                    let layout_py = active_h as i32 - (y_start + layout_ph);
                                    let tex_size = pane.offscreen_ctx.size();
                                    let tex_pw = tex_size.width as i32;
                                    let tex_ph = tex_size.height as i32;
                                    let tex_py = active_h as i32 - y_start - tex_ph;

                                    (layout_pw, layout_ph, x_start, layout_py, tex_pw, tex_ph, x_start, tex_py)
                                };

                                if layout_pw <= 0 || layout_ph <= 0 || tex_pw <= 0 || tex_ph <= 0 { continue; }

                                let is_locked = is_transaction_pending && !transaction_complete;
                                let is_hiding = pending_hides.contains(&pane.id);
                                
                                // Force the use of ghost_tex if the pane is marked for removal in the active transaction
                                let freeze_this_pane = is_locked && pane.ghost_tex_size.is_some() && (pane.is_resizing || is_hiding);

                                if let Some(callback) = pane.offscreen_ctx.render_to_parent_callback() {
                                    if let (Some(comp_fbo), Some(comp_tex)) = (instance.comp_fbo, instance.comp_tex) {
                                        
                                        if pane.ghost_tex.is_none() {
                                            pane.ghost_tex = Some(instance.gl.create_texture().unwrap());
                                        }

                                        if freeze_this_pane {
                                            // 1:1 Map inside the stabilized Scene FBO
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
                                                euclid::default::Point2D::new(tex_px, tex_py),
                                                euclid::default::Size2D::new(tex_pw, tex_ph)
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
                            instance.gl.disable(glow::STENCIL_TEST);

                            // ---------------------------------------------------------
                            // PASS 2: BLIT SCENE FBO TO LIVE WINDOW
                            // ---------------------------------------------------------
                            let _live_w = window_size.width.max(1) as i32;
                            let live_h = window_size.height.max(1) as i32;

                            instance.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                            instance.gl.disable(glow::SCISSOR_TEST);
                            instance.gl.disable(glow::DEPTH_TEST);
                            instance.gl.disable(glow::BLEND); 
                            
                            // Anchor the Scene FBO to the Top-Left of the expanding Winit window
                            instance.gl.viewport(0, live_h - active_h as i32, active_w as i32, active_h as i32);

                            if let Some(program) = instance.comp_program {
                                instance.gl.use_program(Some(program));
                                instance.gl.bind_vertex_array(instance.comp_vao);
                                instance.gl.active_texture(glow::TEXTURE0);
                                instance.gl.bind_texture(glow::TEXTURE_2D, instance.scene_tex);
                                
                                instance.gl.uniform_2_f32(instance.u_comp_size_loc.as_ref(), active_w as f32, active_h as f32);
                                instance.gl.uniform_1_f32(instance.u_comp_radius_loc.as_ref(), 0.0);

                                instance.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                                instance.gl.bind_vertex_array(None);
                                instance.gl.use_program(None);
                            }

                            instance.rendering_context.present();
                            trace!("Rust: [Lifecycle] RedrawRequested DONE");
                        }
                    },            WindowEvent::Resized(size) => {
                let scale_factor = instance.window.scale_factor() as f32;
                
                // 1. Guard against feedback loops: only process if size actually changed
                // (Using a small threshold for floating point stability)
                if let Some(state) = APP_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        if let Some(metadata) = s.window_metadata.get_mut(&uuid) {
                            let last_size = metadata.last_window_size.unwrap_or(winit::dpi::PhysicalSize::new(0, 0));
                            if last_size == size {
                                return;
                            }
                            metadata.last_window_size = Some(size);
                        }
                    }
                }

                info!("Rust: Resized to {}x{}", size.width, size.height);

                instance.recalculate_layout(size, scale_factor);
                
                instance.rendering_context.resize(size);

                let mut msg = Vec::new();
                let logical_w = size.width as f32 / scale_factor;
                let logical_h = size.height as f32 / scale_factor;
                
                info!("Rust: Sending 'resized' event to JS: {}x{} (Logical: {}x{})", size.width, size.height, logical_w, logical_h);
                
                if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                    "event": "resized",
                    "window_id": uuid,
                    "width": size.width,
                    "height": size.height,
                    "scale_factor": scale_factor,
                    "logicalWidth": logical_w,
                    "logicalHeight": logical_h
                })).is_ok() {
                        let mut wrapped = Vec::with_capacity(msg.len() + 1);
                        wrapped.push(MSG_TYPE_DATA);
                        wrapped.extend(msg);
                        self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                }
            },
                    WindowEvent::CursorMoved { position, .. } => {
                        let point = Point2D::new(position.x as f32, position.y as f32);
                        instance.last_mouse_pos = point;
                        let scale_factor = instance.window.scale_factor() as f32;
                        
                        // If mouse is down, route exclusively to the pane that received the Down event
                        if instance.is_mouse_down {
                            if let Some(pane_id) = &instance.last_mouse_down_pane_id {
                                if let Some(pane) = instance.panes.get_mut(pane_id) {
                                    let (px, py) = if pane.anchor != PaneAnchor::None {
                                        (pane.last_physical_rect.origin.x as f32, pane.last_physical_rect.origin.y as f32)
                                    } else {
                                        let physical_rect = pane.rect.scale(scale_factor, scale_factor);
                                        (physical_rect.origin.x.round(), physical_rect.origin.y.round())
                                    };
                                    let translated_point = Point2D::new(point.x - px, point.y - py);
                                    pane.webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
                                        servo::WebViewPoint::Device(translated_point)
                                    )));
                                    return;
                                }
                            }
                        }

                        // Hit-test resize directions first
                        if instance.frameless {
                            let mut hovered_pane = None;
                            let mut sorted_panes: Vec<_> = instance.panes.values().collect();
                            sorted_panes.sort_by_key(|p| -p.z_index); // Reverse Z for hit-testing

                            for pane in sorted_panes {
                                if !pane.is_visible { continue; }
                                let (px, py, pw, ph) = if pane.anchor != PaneAnchor::None {
                                    (
                                        pane.last_physical_rect.origin.x as f32,
                                        pane.last_physical_rect.origin.y as f32,
                                        pane.last_physical_rect.size.width as f32,
                                        pane.last_physical_rect.size.height as f32
                                    )
                                } else {
                                    let physical_rect = pane.rect.scale(scale_factor, scale_factor);
                                    (
                                        physical_rect.origin.x.round(),
                                        physical_rect.origin.y.round(),
                                        physical_rect.size.width.round(),
                                        physical_rect.size.height.round()
                                    )
                                };
                                let rect = euclid::Rect::new(euclid::Point2D::new(px, py), euclid::Size2D::new(pw, ph));

                                if rect.contains(point) {
                                    hovered_pane = Some(pane);
                                    break;
                                }
                            }
                            
                            if let Some(pane) = hovered_pane {
                                // Unified physical coordinate translation
                                let (px, py) = if pane.anchor != PaneAnchor::None {
                                    (pane.last_physical_rect.origin.x as f32, pane.last_physical_rect.origin.y as f32)
                                } else {
                                    let physical_rect = pane.rect.scale(scale_factor, scale_factor);
                                    (physical_rect.origin.x.round(), physical_rect.origin.y.round())
                                };
                                let translated_point = Point2D::new(point.x - px, point.y - py);

                                 // SHADOWING: Check no-drag regions first.
                                 let mut hit_no_drag = false;
                                 for no_drag_region in &pane.no_drag_regions {
                                     if no_drag_region.contains(translated_point) {
                                         hit_no_drag = true;
                                         break;
                                     }
                                 }
                                 
                                 let mut hit_drag = false;
                                 if !hit_no_drag {
                                     for region in &pane.drag_regions {
                                         if region.contains(translated_point) {
                                             hit_drag = true;
                                             break;
                                         }
                                     }
                                 }
                                
                                if hit_no_drag {
                                    if instance.in_resize_border {
                                        instance.window.set_cursor(CursorIcon::Default);
                                        instance.in_resize_border = false;
                                    }
                                } else {
                                    // Only check for resize borders if NOT in a no-drag zone
                                    let size = instance.window.inner_size();
                                    let x = position.x;
                                    let y = position.y;
                                    let w = size.width as f64;
                                    let h = size.height as f64;
                                    let border = 8.0;
                                    
                                    let resize_cursor = if x < border && y < border {
                                        Some(CursorIcon::NwResize)
                                    } else if x > w - border && y < border {
                                        Some(CursorIcon::NeResize)
                                    } else if x < border && y > h - border {
                                        Some(CursorIcon::SwResize)
                                    } else if x > w - border && y > h - border {
                                        Some(CursorIcon::SeResize)
                                    } else if x < border {
                                        Some(CursorIcon::WResize)
                                    } else if x > w - border {
                                        Some(CursorIcon::EResize)
                                    } else if y < border {
                                        Some(CursorIcon::NResize)
                                    } else if y > h - border {
                                        Some(CursorIcon::SResize)
                                    } else {
                                        None
                                    };
                                    
                                    if let Some(icon) = resize_cursor {
                                         instance.window.set_cursor(icon);
                                         instance.in_resize_border = true;
                                     } else {
                                         if hit_drag {
                                             instance.window.set_cursor(winit::window::CursorIcon::Move);
                                         } else {
                                             if instance.in_resize_border {
                                                 instance.window.set_cursor(winit::window::CursorIcon::Default);
                                             }
                                         }
                                         instance.in_resize_border = false;
                                     }
                                }

                                // Route MouseMove to the hovered pane with translated physical coordinates
                                let (px, py) = if pane.anchor != PaneAnchor::None {
                                    (pane.last_physical_rect.origin.x as f32, pane.last_physical_rect.origin.y as f32)
                                } else {
                                    let physical_rect = pane.rect.scale(scale_factor, scale_factor);
                                    (physical_rect.origin.x.round(), physical_rect.origin.y.round())
                                };
                                let translated_point = Point2D::new(point.x - px, point.y - py);

                                pane.webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
                                    servo::WebViewPoint::Device(translated_point)
                                )));
                            }
                        } else {
                            // Standard window: just find hovered pane and route
                            let mut sorted_panes: Vec<_> = instance.panes.values().collect();
                            sorted_panes.sort_by_key(|p| -p.z_index);
                            for pane in sorted_panes {
                                if !pane.is_visible { continue; }
                                let (px, py, pw, ph) = if pane.anchor != PaneAnchor::None {
                                    (
                                        pane.last_physical_rect.origin.x as f32,
                                        pane.last_physical_rect.origin.y as f32,
                                        pane.last_physical_rect.size.width as f32,
                                        pane.last_physical_rect.size.height as f32
                                    )
                                } else {
                                    let physical_rect = pane.rect.scale(scale_factor, scale_factor);
                                    (
                                        physical_rect.origin.x.round(),
                                        physical_rect.origin.y.round(),
                                        physical_rect.size.width.round(),
                                        physical_rect.size.height.round()
                                    )
                                };
                                let rect = euclid::Rect::new(euclid::Point2D::new(px, py), euclid::Size2D::new(pw, ph));

                                if rect.contains(point) {
                                    let translated_point = Point2D::new(point.x - px, point.y - py);
                                    pane.webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
                                        servo::WebViewPoint::Device(translated_point)
                                    )));
                                    break;
                                }
                            }
                        }
                    },
                    WindowEvent::MouseInput { state, button, .. } => {
                        let is_pressed = state == winit::event::ElementState::Pressed;
                        instance.is_mouse_down = is_pressed;
                        let scale_factor = instance.window.scale_factor() as f32;
                        
                        let action = match state {
                            winit::event::ElementState::Pressed => MouseButtonAction::Down,
                            winit::event::ElementState::Released => MouseButtonAction::Up,
                        };
                        let servo_button = match button {
                            winit::event::MouseButton::Left => ServoMouseButton::Left,
                            winit::event::MouseButton::Right => ServoMouseButton::Right,
                            winit::event::MouseButton::Middle => ServoMouseButton::Middle,
                            winit::event::MouseButton::Back => ServoMouseButton::Back,
                            winit::event::MouseButton::Forward => ServoMouseButton::Forward,
                            winit::event::MouseButton::Other(v) => ServoMouseButton::Other(v),
                        };

                        let target_pane_id = if !is_pressed {
                            instance.last_mouse_down_pane_id.take()
                        } else {
                            let mut sorted_panes: Vec<_> = instance.panes.values().collect();
                            sorted_panes.sort_by_key(|p| -p.z_index);
                            
                            let mut found_id = None;
                            for pane in sorted_panes {
                                if !pane.is_visible { continue; }
                                let (px, py, pw, ph) = if pane.anchor != PaneAnchor::None {
                                    (
                                        pane.last_physical_rect.origin.x as f32,
                                        pane.last_physical_rect.origin.y as f32,
                                        pane.last_physical_rect.size.width as f32,
                                        pane.last_physical_rect.size.height as f32
                                    )
                                } else {
                                    let physical_rect = pane.rect.scale(scale_factor, scale_factor);
                                    (
                                        physical_rect.origin.x.round(),
                                        physical_rect.origin.y.round(),
                                        physical_rect.size.width.round(),
                                        physical_rect.size.height.round()
                                    )
                                };
                                let rect = euclid::Rect::new(euclid::Point2D::new(px, py), euclid::Size2D::new(pw, ph));

                                if rect.contains(instance.last_mouse_pos) {
                                    found_id = Some(pane.id.clone());
                                    break;
                                }
                            }
                            if is_pressed {
                                instance.last_mouse_down_pane_id = found_id.clone();
                            }
                            found_id
                        };

                        if let Some(pane_id) = target_pane_id {
                            if let Some(pane) = instance.panes.get_mut(&pane_id) {
                                    // Unified physical coordinate translation
                                    let (px, py) = if pane.anchor != PaneAnchor::None {
                                        (pane.last_physical_rect.origin.x as f32, pane.last_physical_rect.origin.y as f32)
                                    } else {
                                        let physical_rect = pane.rect.scale(scale_factor, scale_factor);
                                        (physical_rect.origin.x.round(), physical_rect.origin.y.round())
                                    };
                                    let translated_point = Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);

                                    if is_pressed && button == winit::event::MouseButton::Left && instance.frameless {
                                        let mut hit_no_drag = false;
                                        for no_drag_region in &pane.no_drag_regions {
                                            if no_drag_region.contains(translated_point) {
                                                hit_no_drag = true;
                                                break;
                                            }
                                        }
                                        
                                        if !hit_no_drag {
                                            let size = instance.window.inner_size();
                                            let x = instance.last_mouse_pos.x as f64;
                                            let y = instance.last_mouse_pos.y as f64;
                                            let w = size.width as f64;
                                            let h = size.height as f64;
                                            let border = 8.0;
                                            
                                            let mut resize_dir = None;
                                            if x < border && y < border {
                                                resize_dir = Some(winit::window::ResizeDirection::NorthWest);
                                            } else if x > w - border && y < border {
                                                resize_dir = Some(winit::window::ResizeDirection::NorthEast);
                                            } else if x < border && y > h - border {
                                                resize_dir = Some(winit::window::ResizeDirection::SouthWest);
                                            } else if x > w - border && y > h - border {
                                                resize_dir = Some(winit::window::ResizeDirection::SouthEast);
                                            } else if x < border {
                                                resize_dir = Some(winit::window::ResizeDirection::West);
                                            } else if x > w - border {
                                                resize_dir = Some(winit::window::ResizeDirection::East);
                                            } else if y < border {
                                                resize_dir = Some(winit::window::ResizeDirection::North);
                                            } else if y > h - border {
                                                resize_dir = Some(winit::window::ResizeDirection::South);
                                            }
                                            
                                            if let Some(dir) = resize_dir {
                                                let translated_point = Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
                                                pane.webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                                                    MouseButtonAction::Up,
                                                    servo_button,
                                                    servo::WebViewPoint::Device(translated_point)
                                                )));
                                                instance.is_mouse_down = false;
                                                instance.last_mouse_down_pane_id = None;
                                                let _ = instance.window.drag_resize_window(dir);
                                                return;
                                            }

                                            let mut hit_drag = false;
                                            for region in &pane.drag_regions {
                                                if region.contains(translated_point) {
                                                    hit_drag = true;
                                                    break;
                                                }
                                            }
                                            
                                            if hit_drag {
                                                info!("Rust: Hit Drag Region at {:?} in pane '{}'", translated_point, pane.id);
                                                let translated_point = Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
                                                pane.webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                                                    MouseButtonAction::Up,
                                                    servo_button,
                                                    servo::WebViewPoint::Device(translated_point)
                                                )));
                                                instance.is_mouse_down = false;
                                                instance.last_mouse_down_pane_id = None;
                                                let _ = instance.window.drag_window();
                                                return;
                                            } else {
                                                trace!("Rust: Drag check FAILED at {:?} for pane '{}'. Drag regions count: {}. No-drag: {}", 
                                                    translated_point, pane.id, pane.drag_regions.len(), pane.no_drag_regions.len());
                                            }
                                        }
                                    }

                                // Focus on click
                                if is_pressed {
                                    instance.active_pane_id = pane.id.clone();
                                }

                                let translated_point = Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
                                pane.webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                                    action,
                                    servo_button,
                                    servo::WebViewPoint::Device(translated_point)
                                )));
                            }
                        }
                    },
                    WindowEvent::MouseWheel { delta, .. } => {
                        let scroll_multiplier = 20.0;
                        let (x, y) = match delta {
                            MouseScrollDelta::LineDelta(x, y) => (x as f64 * scroll_multiplier, y as f64 * scroll_multiplier),
                            MouseScrollDelta::PixelDelta(pos) => (pos.x * scroll_multiplier, pos.y * scroll_multiplier),
                        };
                        let wheel_delta = WheelDelta {
                            x,
                            y,
                            z: 0.0,
                            mode: WheelMode::DeltaPixel,
                        };
                        let scale_factor = instance.window.scale_factor() as f32;

                        if instance.is_mouse_down {
                            if let Some(pane_id) = &instance.last_mouse_down_pane_id {
                                if let Some(pane) = instance.panes.get_mut(pane_id) {
                                    let (px, py) = if pane.anchor != PaneAnchor::None {
                                        (pane.last_physical_rect.origin.x as f32, pane.last_physical_rect.origin.y as f32)
                                    } else {
                                        let physical_rect = pane.rect.scale(scale_factor, scale_factor);
                                        (physical_rect.origin.x.round(), physical_rect.origin.y.round())
                                    };
                                    let translated_point = Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
                                    pane.webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(
                                        wheel_delta,
                                        servo::WebViewPoint::Device(translated_point)
                                    )));
                                    return;
                                }
                            }
                        }

                        let mut sorted_panes: Vec<_> = instance.panes.values().collect();
                        sorted_panes.sort_by_key(|p| -p.z_index);
                        for pane in sorted_panes {
                            if !pane.is_visible { continue; }
                            let (px, py, pw, ph) = if pane.anchor != PaneAnchor::None {
                                (
                                    pane.last_physical_rect.origin.x as f32,
                                    pane.last_physical_rect.origin.y as f32,
                                    pane.last_physical_rect.size.width as f32,
                                    pane.last_physical_rect.size.height as f32
                                )
                            } else {
                                let physical_rect = pane.rect.scale(scale_factor, scale_factor);
                                (
                                    physical_rect.origin.x.round(),
                                    physical_rect.origin.y.round(),
                                    physical_rect.size.width.round(),
                                    physical_rect.size.height.round()
                                )
                            };
                            let rect = euclid::Rect::new(euclid::Point2D::new(px, py), euclid::Size2D::new(pw, ph));

                            if rect.contains(instance.last_mouse_pos) {
                                let translated_point = Point2D::new(instance.last_mouse_pos.x - px, instance.last_mouse_pos.y - py);
                                pane.webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(
                                    wheel_delta,
                                    servo::WebViewPoint::Device(translated_point)
                                )));
                                break;
                            }
                        }
                    },
                    WindowEvent::Moved(position) => {
                        let mut msg = Vec::new();
                        if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                            "event": "moved",
                            "window_id": uuid,
                            "x": position.x,
                            "y": position.y
                        })).is_ok() {
                            let mut wrapped = Vec::with_capacity(msg.len() + 1);
                            wrapped.push(MSG_TYPE_DATA);
                            wrapped.extend(msg);
                            self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                    },
                    WindowEvent::Focused(focused) => {
                        let event_name = if focused { "focused" } else { "unfocused" };
                        let mut msg = Vec::new();
                        if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                            "event": event_name,
                            "window_id": uuid
                        })).is_ok() {
                            let mut wrapped = Vec::with_capacity(msg.len() + 1);
                            wrapped.push(MSG_TYPE_DATA);
                            wrapped.extend(msg);
                            self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                    },
                    WindowEvent::HoveredFile(path) => {
                        let path_str = path.to_string_lossy().into_owned();
                        // Notify Node.js
                        let mut msg = Vec::new();
                        if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                            "event": "file-hover", "window_id": uuid, "path": path_str
                        })).is_ok() {
                            let mut wrapped = Vec::with_capacity(msg.len() + 1);
                            wrapped.push(MSG_TYPE_DATA);
                            wrapped.extend(msg);
                            self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                        // Push to renderer — standard [[channel, payload]] msgpack batch.
                        // to_vec_named is used intentionally (same as to_vec but with
                        // named struct fields) to produce human-readable msgpack maps;
                        // consistent with the serde_json::Value payloads used here.
                        if let Some(senders) = WS_SENDERS.get() {
                            if let Ok(packed) = rmp_serde::encode::to_vec_named(
                                &vec![("file-hover", serde_json::json!({ "path": path_str }))]
                            ) {
                                let msg = axum::extract::ws::Message::Binary(packed.into());
                                for entry in senders.iter() {
                                    if entry.key().starts_with(&format!("{}:", uuid)) || entry.key() == &uuid {
                                        let _ = entry.value().send(msg.clone());
                                    }
                                }
                            }
                        }
                    },
                    WindowEvent::HoveredFileCancelled => {
                        let mut msg = Vec::new();
                        if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                            "event": "file-hover-cancelled", "window_id": uuid
                        })).is_ok() {
                            let mut wrapped = Vec::with_capacity(msg.len() + 1);
                            wrapped.push(MSG_TYPE_DATA);
                            wrapped.extend(msg);
                            self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                        if let Some(senders) = WS_SENDERS.get() {
                            if let Ok(packed) = rmp_serde::encode::to_vec_named(
                                &vec![("file-hover-cancelled", serde_json::json!(null))]
                            ) {
                                let msg = axum::extract::ws::Message::Binary(packed.into());
                                for entry in senders.iter() {
                                    if entry.key().starts_with(&format!("{}:", uuid)) || entry.key() == &uuid {
                                        let _ = entry.value().send(msg.clone());
                                    }
                                }
                            }
                        }
                    },
                    WindowEvent::DroppedFile(path) => {
                        let path_str = path.to_string_lossy().into_owned();
                        // Notify Node.js
                        let mut msg = Vec::new();
                        if rmp_serde::encode::write(&mut msg, &serde_json::json!({
                            "event": "file-drop", "window_id": uuid, "path": path_str
                        })).is_ok() {
                            let mut wrapped = Vec::with_capacity(msg.len() + 1);
                            wrapped.push(MSG_TYPE_DATA);
                            wrapped.extend(msg);
                            self.callback.call((uuid.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                        }
                        // Push to renderer
                        if let Some(senders) = WS_SENDERS.get() {
                            if let Ok(packed) = rmp_serde::encode::to_vec_named(
                                &vec![("file-drop", serde_json::json!({ "path": path_str }))]
                            ) {
                                let msg = axum::extract::ws::Message::Binary(packed.into());
                                for entry in senders.iter() {
                                    if entry.key().starts_with(&format!("{}:", uuid)) || entry.key() == &uuid {
                                        let _ = entry.value().send(msg.clone());
                                    }
                                }
                            }
                        }
                    },
                    WindowEvent::ModifiersChanged(modifiers) => {
                        instance.modifiers = map_winit_modifiers(modifiers);
                    },
                    WindowEvent::KeyboardInput { event, .. } => {
                        let state = match event.state {
                            winit::event::ElementState::Pressed => KeyState::Down,
                            winit::event::ElementState::Released => KeyState::Up,
                        };
                        
                        let key = map_winit_key(&event.logical_key);
                        let code = map_winit_code(event.physical_key);
                        
                        let keyboard_event = ServoKeyboardEvent::new_without_event(
                            state,
                            key,
                            code,
                            Location::Standard,
                            instance.modifiers,
                            event.repeat,
                            false, // is_composing
                        );
                        
                        if let Some(pane) = instance.panes.get(&instance.active_pane_id) {
                            pane.webview.notify_input_event(InputEvent::Keyboard(keyboard_event));
                        }
                    },
                    _ => {}
                }
            }
        }
    }
}

// ------------------------------------------------------------------
// APP STRUCT - Manages global event loop
// ------------------------------------------------------------------


#[napi]
pub struct App {
    // App doesn't hold the proxy directly - it's in the global static
}

#[napi]
impl App {
    #[napi(constructor)]
    pub fn new(callback: ThreadsafeFunction<(String, Vec<napi::bindgen_prelude::Buffer>), ErrorStrategy::Fatal>, profiling: bool, app_identifier: Option<String>, msgpackr_source: String) -> napi::Result<Self> {
        let (proxy_tx, proxy_rx) = crossbeam_channel::bounded(1);
        
        let start_time = Instant::now();
        if profiling {
            eprintln!("[PROFILE] Application starting...");
        }

        // Determine app identifier (default to "lotus" if not provided)
        let app_id = app_identifier.unwrap_or_else(|| "lotus".to_string());

        #[cfg(target_os = "linux")]
        {
            let mode = detect_linux_theme_robust();
            eprintln!("[PROFILE] App::new - Linux Theme Detection Result (Robust): {:?}", mode);
            if mode == dark_light::Mode::Dark {
               env::set_var("GTK_THEME", "Adwaita:dark");
               eprintln!("[PROFILE] Set GTK_THEME=Adwaita:dark");
            } else {
               eprintln!("[PROFILE] Did NOT set GTK_THEME (mode was not Dark)");
            }
        }

        // 1. Initialize global app state
        let app_state = Arc::new(Mutex::new(AppState {
            window_metadata: HashMap::new(),
            window_states: WindowStateManager::new(&app_id),
            ipc_server_port: 0,
            ipc_server_token: Uuid::new_v4().to_string(),
            msgpackr_source,
            profiling,
            _start_time: start_time,
            window_start_times: HashMap::new(),
            vfs: None,
            resource_cache: ByteLimitedLruCache::new(128 * 1024 * 1024), // 128MB limit
        }));
        APP_STATE.set(app_state.clone()).ok();

        let token = match app_state.lock() {
            Ok(s) => s.ipc_server_token.clone(),
            Err(e) => {
                error!("AppState lock poisoned during init: {}", e);
                return Err(napi::Error::from_reason(format!("Internal state lock failed: {}", e)));
            }
        };
        
        // 2. Start Event Loop in dedicated thread
        let event_callback = callback.clone();
        thread::spawn(move || {
            let mut builder = winit::event_loop::EventLoop::<EngineCommand>::with_user_event();
            #[cfg(target_os = "linux")]
            {
                winit::platform::x11::EventLoopBuilderExtX11::with_any_thread(&mut builder, true);
                winit::platform::wayland::EventLoopBuilderExtWayland::with_any_thread(&mut builder, true);
            }
            #[cfg(target_os = "windows")]
            {
                use winit::platform::windows::EventLoopBuilderExtWindows;
                builder.with_any_thread(true);
            }
            
            let event_loop = match builder.build() {
                Ok(el) => el,
                Err(e) => {
                    error!("Failed to build Winit event loop: {}", e);
                    return;
                }
            };
            let proxy = event_loop.create_proxy();
            
            // Store proxy in global static and signal main thread
            EVENT_LOOP_PROXY.set(proxy.clone()).ok();
            let _ = proxy_tx.send(proxy.clone());

            info!("Rust: Starting Event Loop (LotusApp)");
            let mut lotus_app = LotusApp::new(proxy, event_callback);
            let _ = event_loop.run_app(&mut lotus_app);
            
            info!("Rust: Event loop stopped naturally.");
            std::process::exit(0);
        });

        // 3. Wait for proxy to be ready
        let proxy = match proxy_rx.recv() {
            Ok(p) => p,
            Err(e) => {
                error!("Event loop thread failed to start, cannot receive proxy: {}", e);
                return Err(napi::Error::from_reason(
                    format!("Winit event loop failed to start: {}", e)
                ));
            }
        };

        // 4. Start IPC Server (Tokio + Axum)
        let server_proxy = proxy.clone();
        let server_token = token.clone();
        
        // Channel to communicate chosen port back to main thread
        let (port_tx, port_rx) = std::sync::mpsc::channel();
        
        thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    error!("Failed to build tokio runtime: {}", e);
                    return;
                }
            };
            
            rt.block_on(async move {
                let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        error!("Failed to bind Tokio TCP Listener: {}", e);
                        let _ = port_tx.send(0);
                        return;
                    }
                };
                
                let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
                info!("Rust: Tokio/Axum IPC Server listening on port {}", actual_port);
                
                // Update port in state
                if let Some(state) = APP_STATE.get() {
                    if let Ok(mut s) = state.lock() {
                        s.ipc_server_port = actual_port;
                    }
                }
                
                let _ = port_tx.send(actual_port);

                use axum::{
                    routing::{get, post},
                    Router,
                    extract::{State, Path, ws::{WebSocketUpgrade, WebSocket, Message as WsMessage}, Query},
                    response::{IntoResponse, Response},
                    http::{StatusCode, HeaderValue, header},
                    body::Body,
                };
                use tower_http::cors::{CorsLayer, Any};
                use tokio::sync::mpsc;
                use dashmap::DashMap;
                use futures_util::{StreamExt, SinkExt};

                #[derive(serde::Deserialize)]
                struct WsQuery {
                    token: String,
                    id: String,
                    #[serde(rename = "paneId")]
                    pane_id: Option<String>,
                }

                #[derive(Clone)]
                struct ServerState {
                    proxy: winit::event_loop::EventLoopProxy<EngineCommand>,
                    token: String,
                    ws_senders: Arc<DashMap<String, mpsc::UnboundedSender<WsMessage>>>,
                }

                let ws_senders_map: Arc<DashMap<String, mpsc::UnboundedSender<WsMessage>>> = Arc::new(DashMap::new());
                // Expose the sender map globally so the Winit thread can push main→renderer messages.
                WS_SENDERS.set(ws_senders_map.clone()).ok();

                // Initialize the pending-message queue map.
                let ws_pending_map: Arc<DashMap<String, std::collections::VecDeque<Vec<u8>>>> = Arc::new(DashMap::new());
                WS_PENDING.set(ws_pending_map.clone()).ok();

                let state = ServerState {
                    proxy: server_proxy,
                    token: server_token,
                    ws_senders: ws_senders_map,
                };

                let cors = CorsLayer::new()
                    // Only allow connections from localhost origins.
                    // Servo loads pages via lotus-resource:// (origin: null) or http://127.0.0.1:
                    // Both are covered by the null-origin case below plus the 127.0.0.1 check.
                    // Reject any cross-origin request outright.
                    .allow_origin([
                        "http://127.0.0.1".parse::<HeaderValue>().unwrap(),
                        "http://localhost".parse::<HeaderValue>().unwrap(),
                        // lotus-resource:// pages send Origin: null (opaque origin per the Fetch spec).
                        // This is the same value sent by file:// pages — a local attacker who tricks
                        // the user into opening a crafted HTML file in another browser could also have
                        // Origin: null. However, the token query parameter is the real security gate:
                        // it's a random UUID injected only via UserScript into our own windows, so a
                        // third-party page cannot know it. The origin check is defense-in-depth only.
                        "null".parse::<HeaderValue>().unwrap(),
                    ])
                    .allow_methods(Any)
                    .allow_headers(vec![
                        header::CONTENT_TYPE,
                        header::ACCEPT,
                        header::ACCEPT_LANGUAGE,
                        header::HeaderName::from_static("x-lotus-auth"),
                        header::HeaderName::from_static("x-lotus-window-id"),
                    ]);

                let app = Router::new()
                    .route("/batch", post(handle_batch))
                    .route("/ipc/:channel", post(handle_ipc))
                    .route("/resource/*path", get(handle_resource))
                    .route("/ws", get(handle_ws_upgrade))
                    .layer(cors)
                    .with_state(state);

                async fn handle_batch(
                    State(state): State<ServerState>,
                    headers: axum::http::HeaderMap,
                    body: axum::body::Bytes,
                ) -> impl IntoResponse {
                    if headers.get("x-lotus-auth").and_then(|h| h.to_str().ok()) != Some(&state.token) {
                        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
                    }

                    let window_id = headers.get("x-lotus-window-id")
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("unknown")
                        .to_string();



                    let _ = state.proxy.send_event(EngineCommand::IpcMessage(window_id, body.to_vec()));
                    (StatusCode::OK, "ok").into_response()
                }

                async fn handle_ipc(
                    State(state): State<ServerState>,
                    Path(channel): Path<String>,
                    headers: axum::http::HeaderMap,
                    body: axum::body::Bytes,
                ) -> impl IntoResponse {
                    if headers.get("x-lotus-auth").and_then(|h| h.to_str().ok()) != Some(&state.token) {
                        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
                    }

                    let window_id = headers.get("x-lotus-window-id")
                        .and_then(|h| h.to_str().ok())
                        .unwrap_or("unknown")
                        .to_string();

                    let channel_decoded = urlencoding::decode(&channel).unwrap_or(std::borrow::Cow::Borrowed(&channel)).into_owned();
                    let mut msg = Vec::new();
                    if let Ok(_) = rmp_serde::encode::write(&mut msg, &vec![(channel_decoded, body.to_vec())]) {
                        let _ = state.proxy.send_event(EngineCommand::IpcMessage(window_id, msg));
                    }
                    
                    (StatusCode::OK, "ok").into_response()
                }

                async fn handle_resource(
                    Path(path): Path<String>,
                ) -> impl IntoResponse {
                    let root = env::current_dir().unwrap_or_default();
                    let mut full_path = root.clone();
                    full_path.push(path.trim_start_matches('/'));
                    
                    match (full_path.canonicalize(), root.canonicalize()) {
                        (Ok(canonical_full), Ok(canonical_root)) => {
                            if !canonical_full.starts_with(&canonical_root) {
                                warn!("Blocked directory traversal attempt for {:?}", full_path);
                                return (StatusCode::FORBIDDEN, "Forbidden").into_response();
                            }
                            
                            if canonical_full.is_file() {
                                // Use tokio::fs::read so large files (videos, etc.) don't block
                                // a Tokio worker thread for the duration of the disk I/O.
                                match tokio::fs::read(&canonical_full).await {
                                    Ok(content) => {
                                        let mime = mime_guess::from_path(&canonical_full).first_or_octet_stream();
                                        let mut resp = Response::new(Body::from(content));
                                        resp.headers_mut().insert(
                                            header::CONTENT_TYPE,
                                            HeaderValue::from_str(mime.as_ref())
                                                .unwrap_or(HeaderValue::from_static("application/octet-stream")),
                                        );
                                        resp
                                    }
                                    Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Error reading file").into_response(),
                                }
                            } else {
                                (StatusCode::NOT_FOUND, "Not Found").into_response()
                            }
                        },
                        _ => {
                            // Path doesn't exist or is malformed
                            (StatusCode::NOT_FOUND, "Not Found").into_response()
                        }
                    }
                }

                async fn handle_ws_upgrade(
                    ws: WebSocketUpgrade,
                    Query(query): Query<WsQuery>,
                    State(state): State<ServerState>,
                    headers: axum::http::HeaderMap,
                ) -> impl IntoResponse {
                    if query.token != state.token {
                        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
                    }

                    // Validate that the request originates from localhost.
                    // Servo sends lotus-resource:// pages with Origin: null; plain http pages
                    // from 127.0.0.1 send Origin: http://127.0.0.1:<port>.
                    // Anything else (third-party site navigated inside the webview) is rejected.
                    let origin_ok = headers.get("origin")
                        .and_then(|v| v.to_str().ok())
                        .map(|o| o == "null" || o.starts_with("http://127.0.0.1") || o.starts_with("http://localhost"))
                        .unwrap_or(true); // absent Origin (same-origin non-browser fetch) is fine

                    if !origin_ok {
                        warn!("Rust: Rejected WebSocket upgrade from untrusted origin");
                        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
                    }

                    let full_id = match query.pane_id {
                        Some(p) => format!("{}:{}", query.id, p),
                        None => query.id.clone(),
                    };

                    ws.on_upgrade(move |socket| handle_ws_client(socket, full_id, state))
                }

                async fn handle_ws_client(socket: WebSocket, client_id: String, state: ServerState) {
                    info!("Rust: WebSocket client connected for client {}", client_id);
                    let (mut sender, mut receiver) = socket.split();
                    
                    let (tx, mut rx) = mpsc::unbounded_channel();
                    state.ws_senders.insert(client_id.clone(), tx.clone());

                    // Drain any messages buffered while the WS was down (e.g. page reload gap).
                    if let Some(pending) = WS_PENDING.get() {
                        // 1. Drain messages specifically for this pane (e.g. "win:main")
                        if let Some(mut entry) = pending.get_mut(&client_id) {
                            while let Some(frame) = entry.pop_front() {
                                let _ = tx.send(WsMessage::Binary(frame.into()));
                            }
                        }
                        
                        // 2. If this is a "main" pane or equivalent, also drain window-level fallback messages (e.g. "win")
                        let parts: Vec<&str> = client_id.split(':').collect();
                        if parts.len() > 1 && (parts[1] == "main" || parts[1] == parts[0]) {
                            if let Some(mut entry) = pending.get_mut(parts[0]) {
                                while let Some(frame) = entry.pop_front() {
                                    let _ = tx.send(WsMessage::Binary(frame.into()));
                                }
                            }
                        }
                    }

                    let send_task = async move {
                        while let Some(msg) = rx.recv().await {
                            if sender.send(msg).await.is_err() {
                                break;
                            }
                        }
                    };

                    let proxy = state.proxy.clone();
                    let client_id_clone = client_id.clone();
                    let recv_task = async move {
                        let mut batch_buffer: Vec<Vec<u8>> = Vec::with_capacity(32);
                        // Cumulative byte size of messages currently in the batch buffer.
                        // Flush early if this exceeds 5 MB to avoid one huge event-loop message.
                        let mut batch_bytes: usize = 0;
                        const BATCH_FLUSH_SIZE_BYTES: usize = 5 * 1024 * 1024; // 5 MB
                        
                        loop {
                            tokio::select! {
                                // Default branch: Read an incoming chunk if we haven't reached the deadline yet
                                msg_opt = receiver.next() => {
                                    match msg_opt {
                                        Some(Ok(WsMessage::Binary(bin))) => {
                                            batch_bytes += bin.len();
                                            batch_buffer.push(bin);
                                        }
                                        Some(Ok(WsMessage::Text(txt))) => {
                                            batch_bytes += txt.len();
                                            batch_buffer.push(txt.into_bytes());
                                        }
                                        Some(Err(_)) | None => {
                                            if !batch_buffer.is_empty() {
                                                let msgs = std::mem::take(&mut batch_buffer);
                                                let _ = proxy.send_event(EngineCommand::IpcMessages(client_id_clone.clone(), msgs));
                                            }
                                            break;
                                        }
                                        _ => {}
                                    }
                                    
                                    // Flush if count limit OR byte-size limit reached.
                                    if batch_buffer.len() >= 32 || batch_bytes >= BATCH_FLUSH_SIZE_BYTES {
                                        batch_bytes = 0;
                                        let msgs = std::mem::take(&mut batch_buffer);
                                        let _ = proxy.send_event(EngineCommand::IpcMessages(client_id_clone.clone(), msgs));
                                    }
                                }
                                
                                // Flush with a short idle sleep: gives the OS a chance to
                                // deliver a burst of follow-on messages in one batch,
                                // without adding more than 0.05ms to solo-message RTTs.
                                // (invoke() round-trips are the most latency-sensitive path.)
                                _ = tokio::time::sleep(std::time::Duration::from_micros(50)), if !batch_buffer.is_empty() => {
                                    batch_bytes = 0;
                                    let msgs = std::mem::take(&mut batch_buffer);
                                    let _ = proxy.send_event(EngineCommand::IpcMessages(client_id_clone.clone(), msgs));
                                }
                            }
                        }
                    };

                    tokio::select! {
                        _ = send_task => {},
                        _ = recv_task => {},
                    }
                    
                    info!("Rust: WebSocket client disconnected for client {}", client_id);
                    state.ws_senders.remove(&client_id); // Ensure dashmap unregisters this window id
                }

                if let Err(e) = axum::serve(listener, app).await {
                    error!("Axum server error: {}", e);
                }
            });
        });

        // 5. Send Initial ready event to Node.js
        let ready_token = token.clone();
        let ready_callback = callback.clone();
        
        // Synchronously wait up to 1s for port to be assigned from axum.
        // This safely pauses the V8 event loop for the ~1ms it takes to bind the port,
        // guaranteeing that APP_STATE is populated before any CreateWindow commands arrive.
        let port = port_rx.recv_timeout(std::time::Duration::from_millis(1000)).unwrap_or(0);

        thread::spawn(move || {
            let mut ready_msg = Vec::new();
            if rmp_serde::encode::write(&mut ready_msg, &serde_json::json!({
                "event": "app-ready", 
                "ipc_port": port, 
                "ipc_token": ready_token
            })).is_ok() {
                let mut wrapped = Vec::with_capacity(ready_msg.len() + 1);
                wrapped.push(MSG_TYPE_DATA);
                wrapped.extend(ready_msg);
                ready_callback.call(("global".to_string(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
            }
        });

        Ok(App {})
    }

    #[napi]
    pub fn init_vfs(&self) -> napi::Result<()> {
        if let Some(state) = APP_STATE.get() {
            if let Ok(mut s) = state.lock() {
                if s.vfs.is_none() {
                    if let Some(vfs) = EncryptedVfs::init() {
                        s.vfs = Some(Arc::new(vfs));
                        info!("Rust: Encrypted VFS initialized successfully.");
                    } else {
                        warn!("Rust: Encrypted VFS initialization skipped (no VFS or shards found).");
                    }
                }
            }
        }
        Ok(())
    }

    #[napi]
    pub fn quit(&self) {
        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
            let _ = proxy.send_event(EngineCommand::Quit);
        }
    }
}

// ------------------------------------------------------------------
// CREATE WINDOW FUNCTION
// ------------------------------------------------------------------

#[napi]
pub fn create_window(options: WindowOptions) -> WindowHandle {
    let window_id = options.id.clone().unwrap_or_else(|| Uuid::new_v4().to_string());
    
    if let Some(proxy) = EVENT_LOOP_PROXY.get() {
        let _ = proxy.send_event(EngineCommand::CreateWindow(options, window_id.clone()));
    }
    
    WindowHandle {
        id: window_id,
    }
}
