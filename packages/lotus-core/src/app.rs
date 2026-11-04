use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use std::path::PathBuf;

use once_cell::sync::OnceCell;
use log::{info, error, debug, trace, warn};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize as WinitPhysicalSize;
use dpi::PhysicalSize as ServoPhysicalSize;
use winit::event::{WindowEvent, MouseScrollDelta};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy, ControlFlow};
use winit::window::{Window, WindowId, CursorIcon};
use winit::raw_window_handle::{HasWindowHandle, HasDisplayHandle};

#[cfg(target_os = "macos")]
use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
#[cfg(target_os = "linux")]
use winit::platform::x11::WindowAttributesExtX11;
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;

use servo::{
    ServoBuilder,
    WebViewBuilder, WindowRenderingContext, RenderingContext,
    InputEvent, KeyboardEvent as ServoKeyboardEvent,
    KeyState, Location, Modifiers,
    MouseButton as ServoMouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
    WheelEvent, WheelDelta, WheelMode,
    UserContentManager, UserScript,
    DeviceIndependentPixel, DevicePixel,
};
use euclid::{Point2D, Scale};
use dark_light;

use crate::window::{
    WindowInstance, PaneInstance, WindowMetadata, DragRegionPayload, SelectPromptWrapper,
    ActiveSelectPrompt, WindowOptions, PaneOptions, ScrollbarConfig,
    map_winit_key, map_winit_code, map_winit_modifiers,
};
use crate::layout::{PaneAnchor, StagedLayout};
use crate::server::{WS_SENDERS, WS_PENDING};
use crate::delegate::LotusPaneDelegate;
use crate::gl::{self, RemotePaneConsumer};
use crate::vfs::{ByteLimitedLruCache, EncryptedVfs};
use crate::window_state::{self, WindowStateManager};
use crate::platform;

use napi::threadsafe_function::{ErrorStrategy, ThreadsafeFunction, ThreadsafeFunctionCallMode};

pub(crate) static EVENT_LOOP_PROXY: OnceCell<EventLoopProxy<EngineCommand>> = OnceCell::new();

// Global app state (thread-safe metadata only, no Rc types)
pub(crate) static APP_STATE: OnceCell<Arc<Mutex<AppState>>> = OnceCell::new();

pub const MSG_TYPE_CONTROL: u8 = 0x01;
pub const MSG_TYPE_DATA: u8 = 0x02;

static TRUSTED_ORIGINS: OnceCell<Arc<dashmap::DashSet<String>>> = OnceCell::new();

pub(crate) fn get_trusted_origins() -> Arc<dashmap::DashSet<String>> {
    TRUSTED_ORIGINS.get_or_init(|| {
        let set = Arc::new(dashmap::DashSet::new());
        set.insert("lotus-resource://localhost".to_string());
        set
    }).clone()
}

pub(crate) fn normalize_origin(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.starts_with("lotus-resource:") {
        let after_scheme = trimmed.trim_start_matches("lotus-resource:").trim_start_matches('/');
        let host = after_scheme.split(&['/', '?', '#'][..]).next().unwrap_or("");
        let host = if host.is_empty() { "localhost" } else { host };
        return Some(format!("lotus-resource://{}", host));
    }

    if let Ok(parsed) = url::Url::parse(trimmed) {
        let scheme = parsed.scheme();
        if scheme == "http" || scheme == "https" || scheme == "ws" || scheme == "wss" {
            if let Some(host) = parsed.host_str() {
                let port_suffix = match parsed.port() {
                    Some(p) => format!(":{}", p),
                    None => String::new(),
                };
                return Some(format!("{}://{}{}", scheme, host, port_suffix));
            }
        }
    }

    None
}

#[inline]
pub(crate) fn fast_exit(code: i32) -> ! {
    #[cfg(unix)]
    unsafe {
        libc::_exit(code);
    }
    #[cfg(not(unix))]
    std::process::exit(code);
}

#[cfg(target_os = "linux")]
pub fn detect_linux_theme_robust() -> dark_light::Mode {
    // 1. Check environment variables populated by the Node Host launcher
    if let Ok(theme) = std::env::var("LOTUS_SYSTEM_THEME") {
        if theme.eq_ignore_ascii_case("dark") {
            return dark_light::Mode::Dark;
        } else if theme.eq_ignore_ascii_case("light") {
            return dark_light::Mode::Light;
        }
    }
    if let Ok(gtk) = std::env::var("GTK_THEME") {
        let lower = gtk.to_lowercase();
        if lower.contains("-dark") || lower.contains(":dark") {
            return dark_light::Mode::Dark;
        }
    }

    // 2. Delegate to the standard dark_light crate without invoking any external subprocesses (e.g. gsettings)
    dark_light::detect()
}

pub(crate) struct AppState {
    pub(crate) window_metadata: HashMap<String, WindowMetadata>,
    pub(crate) window_states: WindowStateManager,
    pub(crate) ipc_server_port: u16,
    pub(crate) ipc_server_token: String,
    pub(crate) msgpackr_source: String,
    pub(crate) profiling: bool,
    pub(crate) _start_time: Instant,
    pub(crate) window_start_times: HashMap<String, Instant>,
    pub(crate) vfs: Option<Arc<EncryptedVfs>>,
    pub(crate) resource_cache: ByteLimitedLruCache,
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

const SCROLLBAR_SCRIPT: &str = r#"
(function() {
    if (window.__lotus_scrollbar_initialized) return;
    window.__lotus_scrollbar_initialized = true;

    const config = window.__LOTUS_SCROLLBAR_CONFIG__ || {
        theme: 'auto',
        autoHide: true,
        width: 10
    };

    const width = Math.max(2, config.width || 10);
    const autoHide = config.autoHide !== false;
    const theme = config.theme || 'auto';

    let trackY = null;
    let thumbY = null;
    let trackX = null;
    let thumbX = null;
    let dragBlocker = null;

    let isDraggingY = false;
    let isDraggingX = false;
    let fadeTimeout = null;
    let resizeObserver = null;

    function enableDragBlocker() {
        if (!dragBlocker) {
            dragBlocker = document.createElement('div');
            dragBlocker.id = '__lotus_scroll_drag_blocker';
            dragBlocker.style.cssText = 'position:fixed!important;top:0!important;left:0!important;width:100%!important;height:100%!important;z-index:2147483647!important;background:transparent!important;cursor:default!important;-webkit-app-region:no-drag!important;pointer-events:auto!important;';
            dragBlocker.setAttribute('data-lotus-drag', 'false');
        }
        if (!dragBlocker.parentNode && document.documentElement) {
            document.documentElement.appendChild(dragBlocker);
        }
        document.documentElement.classList.add('__lotus_scrolling_active');
    }

    function disableDragBlocker() {
        if (dragBlocker && dragBlocker.parentNode) {
            dragBlocker.parentNode.removeChild(dragBlocker);
        }
        if (document.documentElement) {
            document.documentElement.classList.remove('__lotus_scrolling_active');
        }
    }

    function showScrollbars() {
        if (trackY && trackY.style.display !== 'none') trackY.style.opacity = '1';
        if (trackX && trackX.style.display !== 'none') trackX.style.opacity = '1';
    }

    function scheduleFade() {
        if (!autoHide || isDraggingY || isDraggingX) return;
        clearTimeout(fadeTimeout);
        fadeTimeout = setTimeout(() => {
            if (!isDraggingY && !isDraggingX) {
                if (trackY) trackY.style.opacity = '0';
                if (trackX) trackX.style.opacity = '0';
            }
        }, 800);
    }

    function updateGeometry() {
        if (!trackY || !trackX) return;
        const scrollEl = document.scrollingElement || document.documentElement;
        if (!scrollEl) return;

        const clientH = window.innerHeight;
        const clientW = window.innerWidth;
        const scrollH = scrollEl.scrollHeight;
        const scrollW = scrollEl.scrollWidth;

        const hasOverflowY = scrollH > clientH + 1;
        const hasOverflowX = scrollW > clientW + 1;

        // Collision avoidance when both scrollbars appear
        if (hasOverflowY && hasOverflowX) {
            trackY.style.height = `calc(100% - ${width}px)`;
            trackX.style.width = `calc(100% - ${width}px)`;
        } else {
            trackY.style.height = '100%';
            trackX.style.width = '100%';
        }

        // Vertical scrollbar geometry
        if (!hasOverflowY) {
            trackY.style.display = 'none';
        } else {
            trackY.style.display = 'block';
            const availableTrackH = hasOverflowX ? clientH - width : clientH;
            const thumbH = Math.max(24, Math.round((availableTrackH / scrollH) * availableTrackH));
            const maxScrollTop = scrollH - clientH;
            const maxThumbTop = Math.max(0, availableTrackH - thumbH);
            const thumbTop = maxScrollTop > 0 ? Math.round((scrollEl.scrollTop / maxScrollTop) * maxThumbTop) : 0;

            thumbY.dataset.top = String(thumbTop);
            thumbY.dataset.height = String(thumbH);
            thumbY.style.height = `${thumbH}px`;
            thumbY.style.transform = `translate3d(0, ${thumbTop}px, 0)`;
        }

        // Horizontal scrollbar geometry
        if (!hasOverflowX) {
            trackX.style.display = 'none';
        } else {
            trackX.style.display = 'block';
            const availableTrackW = hasOverflowY ? clientW - width : clientW;
            const thumbW = Math.max(24, Math.round((availableTrackW / scrollW) * availableTrackW));
            const maxScrollLeft = scrollW - clientW;
            const maxThumbLeft = Math.max(0, availableTrackW - thumbW);
            const thumbLeft = maxScrollLeft > 0 ? Math.round((scrollEl.scrollLeft / maxScrollLeft) * maxThumbLeft) : 0;

            thumbX.dataset.left = String(thumbLeft);
            thumbX.dataset.width = String(thumbW);
            thumbX.style.width = `${thumbW}px`;
            thumbX.style.transform = `translate3d(${thumbLeft}px, 0, 0)`;
        }
    }

    function initScrollbars() {
        const root = document.documentElement;
        if (!root) {
            document.addEventListener('DOMContentLoaded', initScrollbars, { once: true });
            return;
        }

        if (document.getElementById('__lotus_scroll_track_y')) return;

        // Scoped Overlay Scrollbar Styles
        const style = document.createElement('style');
        style.id = '__lotus_scrollbar_styles';
        style.textContent = `
            #__lotus_scroll_track_y, #__lotus_scroll_track_x {
                position: fixed !important;
                z-index: 2147483646 !important;
                pointer-events: auto !important;
                user-select: none !important;
                -webkit-user-select: none !important;
                -webkit-app-region: no-drag !important;
                background: transparent !important;
                box-sizing: border-box !important;
                opacity: 0;
                transition: opacity 0.25s ease-out;
            }
            #__lotus_scroll_track_y {
                top: 0 !important;
                right: 0 !important;
                width: ${width}px !important;
                height: 100% !important;
                transition: width 0.15s ease, opacity 0.25s ease-out;
            }
            #__lotus_scroll_track_y:hover,
            #__lotus_scroll_track_y.active {
                width: ${Math.max(width, 12)}px !important;
            }
            #__lotus_scroll_track_x {
                bottom: 0 !important;
                left: 0 !important;
                height: ${width}px !important;
                width: 100% !important;
                transition: height 0.15s ease, opacity 0.25s ease-out;
            }
            #__lotus_scroll_track_x:hover,
            #__lotus_scroll_track_x.active {
                height: ${Math.max(width, 12)}px !important;
            }
            #__lotus_scroll_thumb_y, #__lotus_scroll_thumb_x {
                position: absolute !important;
                border-radius: ${width}px !important;
                box-sizing: border-box !important;
                -webkit-app-region: no-drag !important;
                pointer-events: auto !important;
                cursor: default !important;
                will-change: transform;
                transition: width 0.15s ease, height 0.15s ease, background 0.15s ease;
            }
            #__lotus_scroll_thumb_y {
                top: 0 !important;
                right: 2px !important;
                left: auto !important;
                width: ${Math.max(4, width - 4)}px !important;
            }
            #__lotus_scroll_track_y:hover #__lotus_scroll_thumb_y,
            #__lotus_scroll_track_y.active #__lotus_scroll_thumb_y {
                width: ${Math.max(width - 2, 8)}px !important;
            }
            #__lotus_scroll_thumb_x {
                left: 0 !important;
                bottom: 2px !important;
                top: auto !important;
                height: ${Math.max(4, width - 4)}px !important;
            }
            #__lotus_scroll_track_x:hover #__lotus_scroll_thumb_x,
            #__lotus_scroll_track_x.active #__lotus_scroll_thumb_x {
                height: ${Math.max(width - 2, 8)}px !important;
            }

            /* Disable iframes pointer events while dragging scrollbars */
            .__lotus_scrolling_active iframe {
                pointer-events: none !important;
            }

            /* Light theme */
            [data-theme="light"] #__lotus_scroll_thumb_y,
            [data-theme="light"] #__lotus_scroll_thumb_x,
            .__lotus_theme_light #__lotus_scroll_thumb_y,
            .__lotus_theme_light #__lotus_scroll_thumb_x {
                background: rgba(0, 0, 0, 0.35) !important;
            }
            [data-theme="light"] #__lotus_scroll_thumb_y:hover,
            [data-theme="light"] #__lotus_scroll_track_y.active #__lotus_scroll_thumb_y,
            [data-theme="light"] #__lotus_scroll_thumb_x:hover,
            [data-theme="light"] #__lotus_scroll_track_x.active #__lotus_scroll_thumb_x {
                background: rgba(0, 0, 0, 0.65) !important;
            }

            /* Dark theme */
            [data-theme="dark"] #__lotus_scroll_thumb_y,
            [data-theme="dark"] #__lotus_scroll_thumb_x,
            .__lotus_theme_dark #__lotus_scroll_thumb_y,
            .__lotus_theme_dark #__lotus_scroll_thumb_x {
                background: rgba(255, 255, 255, 0.35) !important;
            }
            [data-theme="dark"] #__lotus_scroll_thumb_y:hover,
            [data-theme="dark"] #__lotus_scroll_track_y.active #__lotus_scroll_thumb_y,
            [data-theme="dark"] #__lotus_scroll_thumb_x:hover,
            [data-theme="dark"] #__lotus_scroll_track_x.active #__lotus_scroll_thumb_x {
                background: rgba(255, 255, 255, 0.65) !important;
            }

            /* Neutral fallback */
            #__lotus_scroll_thumb_y, #__lotus_scroll_thumb_x {
                background: rgba(127, 127, 127, 0.45) !important;
            }
            #__lotus_scroll_thumb_y:hover,
            #__lotus_scroll_track_y.active #__lotus_scroll_thumb_y,
            #__lotus_scroll_thumb_x:hover,
            #__lotus_scroll_track_x.active #__lotus_scroll_thumb_x {
                background: rgba(127, 127, 127, 0.8) !important;
            }
        `;
        root.appendChild(style);

        // Vertical Track & Thumb
        trackY = document.createElement('div');
        trackY.id = '__lotus_scroll_track_y';
        trackY.setAttribute('data-lotus-drag', 'false');

        thumbY = document.createElement('div');
        thumbY.id = '__lotus_scroll_thumb_y';
        thumbY.setAttribute('data-lotus-drag', 'false');
        trackY.appendChild(thumbY);

        // Horizontal Track & Thumb
        trackX = document.createElement('div');
        trackX.id = '__lotus_scroll_track_x';
        trackX.setAttribute('data-lotus-drag', 'false');

        thumbX = document.createElement('div');
        thumbX.id = '__lotus_scroll_thumb_x';
        thumbX.setAttribute('data-lotus-drag', 'false');
        trackX.appendChild(thumbX);

        if (theme === 'light') {
            trackY.classList.add('__lotus_theme_light');
            trackX.classList.add('__lotus_theme_light');
        } else if (theme === 'dark') {
            trackY.classList.add('__lotus_theme_dark');
            trackX.classList.add('__lotus_theme_dark');
        }

        root.appendChild(trackY);
        root.appendChild(trackX);

        // Thumb Y Drag Handling
        thumbY.addEventListener('mousedown', (e) => {
            e.preventDefault();
            e.stopPropagation();

            const scrollEl = document.scrollingElement || document.documentElement;
            const clientH = window.innerHeight;
            const scrollH = scrollEl.scrollHeight;
            const hasOverflowX = scrollEl.scrollWidth > window.innerWidth + 1;
            const availableTrackH = hasOverflowX ? clientH - width : clientH;
            const thumbH = Math.max(24, Math.round((availableTrackH / scrollH) * availableTrackH));
            const maxThumbTop = Math.max(0, availableTrackH - thumbH);
            const maxScrollTop = scrollH - clientH;
            if (maxThumbTop <= 0 || maxScrollTop <= 0) return;

            const startMouseY = e.clientY;
            const startScrollTop = scrollEl.scrollTop;
            const ratio = maxScrollTop / maxThumbTop;

            isDraggingY = true;
            clearTimeout(fadeTimeout);
            trackY.classList.add('active');
            showScrollbars();
            enableDragBlocker();

            function onMouseMove(moveEvent) {
                const deltaY = moveEvent.clientY - startMouseY;
                scrollEl.scrollTop = startScrollTop + deltaY * ratio;
            }

            function onMouseUp() {
                isDraggingY = false;
                trackY.classList.remove('active');
                disableDragBlocker();
                window.removeEventListener('mousemove', onMouseMove, true);
                window.removeEventListener('mouseup', onMouseUp, true);
                scheduleFade();
            }

            window.addEventListener('mousemove', onMouseMove, true);
            window.addEventListener('mouseup', onMouseUp, true);
        });

        // Track Y Click (Deadzone checked jump)
        trackY.addEventListener('mousedown', (e) => {
            if (e.target === thumbY) return;
            e.preventDefault();
            e.stopPropagation();

            const scrollEl = document.scrollingElement || document.documentElement;
            const clientH = window.innerHeight;
            const clickY = e.clientY;
            const currentThumbTop = parseFloat(thumbY.dataset.top || '0');
            const thumbH = parseFloat(thumbY.dataset.height || '24');

            if (clickY < currentThumbTop - 2) {
                scrollEl.scrollTop -= clientH * 0.9;
            } else if (clickY > currentThumbTop + thumbH + 2) {
                scrollEl.scrollTop += clientH * 0.9;
            }
        });

        // Thumb X Drag Handling
        thumbX.addEventListener('mousedown', (e) => {
            e.preventDefault();
            e.stopPropagation();

            const scrollEl = document.scrollingElement || document.documentElement;
            const clientW = window.innerWidth;
            const scrollW = scrollEl.scrollWidth;
            const hasOverflowY = scrollEl.scrollHeight > window.innerHeight + 1;
            const availableTrackW = hasOverflowY ? clientW - width : clientW;
            const thumbW = Math.max(24, Math.round((availableTrackW / scrollW) * availableTrackW));
            const maxThumbLeft = Math.max(0, availableTrackW - thumbW);
            const maxScrollLeft = scrollW - clientW;
            if (maxThumbLeft <= 0 || maxScrollLeft <= 0) return;

            const startMouseX = e.clientX;
            const startScrollLeft = scrollEl.scrollLeft;
            const ratio = maxScrollLeft / maxThumbLeft;

            isDraggingX = true;
            clearTimeout(fadeTimeout);
            trackX.classList.add('active');
            showScrollbars();
            enableDragBlocker();

            function onMouseMove(moveEvent) {
                const deltaX = moveEvent.clientX - startMouseX;
                scrollEl.scrollLeft = startScrollLeft + deltaX * ratio;
            }

            function onMouseUp() {
                isDraggingX = false;
                trackX.classList.remove('active');
                disableDragBlocker();
                window.removeEventListener('mousemove', onMouseMove, true);
                window.removeEventListener('mouseup', onMouseUp, true);
                scheduleFade();
            }

            window.addEventListener('mousemove', onMouseMove, true);
            window.addEventListener('mouseup', onMouseUp, true);
        });

        // Track X Click (Deadzone checked jump)
        trackX.addEventListener('mousedown', (e) => {
            if (e.target === thumbX) return;
            e.preventDefault();
            e.stopPropagation();

            const scrollEl = document.scrollingElement || document.documentElement;
            const clientW = window.innerWidth;
            const clickX = e.clientX;
            const currentThumbLeft = parseFloat(thumbX.dataset.left || '0');
            const thumbW = parseFloat(thumbX.dataset.width || '24');

            if (clickX < currentThumbLeft - 2) {
                scrollEl.scrollLeft -= clientW * 0.9;
            } else if (clickX > currentThumbLeft + thumbW + 2) {
                scrollEl.scrollLeft += clientW * 0.9;
            }
        });

        // Hover & Edge detection
        trackY.addEventListener('mouseenter', showScrollbars);
        trackY.addEventListener('mouseleave', scheduleFade);
        trackX.addEventListener('mouseenter', showScrollbars);
        trackX.addEventListener('mouseleave', scheduleFade);

        window.addEventListener('mousemove', (e) => {
            if (window.innerWidth - e.clientX <= Math.max(20, width + 10) ||
                window.innerHeight - e.clientY <= Math.max(20, width + 10)) {
                showScrollbars();
                scheduleFade();
            }
        }, { passive: true });

        // Scroll listener
        window.addEventListener('scroll', () => {
            updateGeometry();
            showScrollbars();
            scheduleFade();
        }, { passive: true });

        // Window resize
        window.addEventListener('resize', () => {
            updateGeometry();
            showScrollbars();
            scheduleFade();
        }, { passive: true });

        // Observers
        if (window.ResizeObserver) {
            resizeObserver = new ResizeObserver(() => {
                updateGeometry();
            });
            resizeObserver.observe(root);
            if (document.body) {
                resizeObserver.observe(document.body);
            }
        }

        updateGeometry();
        if (!autoHide) {
            showScrollbars();
        } else {
            showScrollbars();
            scheduleFade();
        }
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', () => {
            initScrollbars();
            if (document.body && resizeObserver) {
                resizeObserver.observe(document.body);
            }
            updateGeometry();
        }, { once: true });
    } else {
        initScrollbars();
    }
})();
"#;

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
    CreatePane(String, String, String, euclid::Rect<f32, servo::DeviceIndependentPixel>, i32, PaneAnchor, u32, bool), // window_id, pane_id, url, rect, z_index, anchor, dock_order, visible
    CreateRemotePane {
        window_id: String,
        pane_id: String,
        ro_fd: std::os::unix::io::RawFd,
        shm_size: usize,
        ack_rw_fd: std::os::unix::io::RawFd,
        rect: euclid::Rect<f32, servo::DeviceIndependentPixel>,
        z_index: i32,
        anchor: PaneAnchor,
        dock_order: u32,
        visible: bool,
    },
    RemovePane(String, String), // window_id, pane_id
    SetPaneRect(String, String, euclid::Rect<f32, servo::DeviceIndependentPixel>), // window_id, pane_id, rect
    SetPaneVisible(String, String, bool), // window_id, pane_id, visible
    FocusPane(String, String), // window_id, pane_id

    // Embedder control commands
    ShowSelectControl {
        window_id: String,
        pane_id: String,
        prompt: SelectPromptWrapper,
    },
    HideSelectControl {
        window_id: String,
        control_id: servo::EmbedderControlId,
    },
    UpdateSelectControl {
        window_id: String,
        control_id: String,
        selected_indices: Vec<u32>,
    },
    SubmitSelectControl {
        window_id: String,
        control_id: String,
        selected_indices: Vec<u32>,
    },
    DismissSelectControl {
        window_id: String,
        control_id: String,
    },
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


// ------------------------------------------------------------------
// WAKER STRATEGY
// ------------------------------------------------------------------

#[derive(Clone)]
pub struct LotusWaker(EventLoopProxy<EngineCommand>, Arc<AtomicBool>);

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

pub struct LotusApp {
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
    pub fn new(
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

                let scrollbar_config = ScrollbarConfig {
                    enabled: options.scrollbars_enabled.unwrap_or(true),
                    theme: options.scrollbar_theme.unwrap_or_else(|| "auto".to_string()),
                    auto_hide: options.scrollbar_auto_hide.unwrap_or(true),
                    width: options.scrollbar_width.unwrap_or(8),
                };

                let mut effective_trusted_origins: Vec<String> = Vec::new();
                if let Some(list) = &options.trusted_origins {
                    for o in list {
                        if let Some(norm) = normalize_origin(o) {
                            if !effective_trusted_origins.contains(&norm) {
                                effective_trusted_origins.push(norm);
                            }
                        }
                    }
                    if options.root.is_some() && !effective_trusted_origins.contains(&"lotus-resource://localhost".to_string()) {
                        effective_trusted_origins.push("lotus-resource://localhost".to_string());
                    }
                } else {
                    effective_trusted_origins.push("lotus-resource://localhost".to_string());
                    if let Some(initial) = &options.initial_url {
                        if let Some(norm) = normalize_origin(initial) {
                            if !effective_trusted_origins.contains(&norm) {
                                effective_trusted_origins.push(norm);
                            }
                        }
                    }
                }

                let trusted_set = get_trusted_origins();
                for o in &effective_trusted_origins {
                    trusted_set.insert(o.clone());
                }

                let origins_json = serde_json::to_string(&effective_trusted_origins)
                    .unwrap_or_else(|_| "[\"lotus-resource://localhost\"]".to_string());

                // UNIFIED PIPELINE: Every window is a multi-pane layout.
                // If options.panes is empty, synthesize a single 'main' fill pane.
                let mut effective_panes = options.panes.clone();
                if effective_panes.is_empty() {
                    effective_panes.push(PaneOptions {
                        id: "main".to_string(),
                        url: options.initial_url.clone().unwrap_or_else(|| "about:blank".to_string()),
                        x: 0.0,
                        y: 0.0,
                        width: size.width as f64 / hidpi_scale_factor_val as f64,
                        height: size.height as f64 / hidpi_scale_factor_val as f64,
                        z_index: 0,
                        visible: true,
                        anchor: Some(if options.auto_resize_main { 1 } else { 0 }),
                        dock_order: Some(0),
                    });
                }

                for (i, pane_opt) in effective_panes.iter().enumerate() {
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
                    let p_port_script = format!(
                        r#"(function() {{
                            const trusted = {};
                            const loc = window.location;
                            const effectiveOrigin = (loc.origin && loc.origin !== 'null') ? loc.origin : (loc.protocol + '//' + (loc.host || 'localhost'));
                            if (trusted.indexOf(effectiveOrigin) !== -1 || (loc.protocol === 'lotus-resource:' && trusted.indexOf('lotus-resource://localhost') !== -1)) {{
                                window.lotus = window.lotus || {{}};
                                window.lotus.port = {};
                                window.lotus.token = '{}';
                                window.lotus.id = '{}';
                                window.lotus.paneId = '{}';
                            }}
                        }})();"#,
                        origins_json, port, token, window_id, pane_opt.id
                    );
                    p_ucm.add_script(Rc::new(UserScript::from(p_port_script.as_str())));
                    
                    let theme_str = match mode { dark_light::Mode::Dark => "dark", _ => "light" };
                    let theme_script = format!(r#"
                        window.lotus.theme = '{}';
                        try {{ document.documentElement.dataset.theme = window.lotus.theme; }} catch(e) {{}}
                    "#, theme_str);
                    p_ucm.add_script(Rc::new(UserScript::from(theme_script.as_str())));
                    p_ucm.add_script(Rc::new(UserScript::from(DRAG_REGION_SCRIPT)));
                    
                    if !pane_opt.id.starts_with("__lotus_") && scrollbar_config.enabled {
                        let cfg_script = format!(
                            "window.__LOTUS_SCROLLBAR_CONFIG__ = {{ theme: '{}', autoHide: {}, width: {} }};",
                            scrollbar_config.theme,
                            scrollbar_config.auto_hide,
                            scrollbar_config.width
                        );
                        p_ucm.add_script(Rc::new(UserScript::from(cfg_script.as_str())));
                        p_ucm.add_script(Rc::new(UserScript::from(SCROLLBAR_SCRIPT)));
                    }
                    
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
                        ghost_tex: None,
                        ghost_tex_size: None,
                        is_remote: false,
                        remote_consumer: None,
                    };
                    panes.insert(pane_opt.id.clone(), pane);
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
                    active_select: None,
                    scrollbar_config,
                    trusted_origins: effective_trusted_origins,
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
                    "logicalHeight": actual_size.height as f32 / actual_scale,
                    "maximized": window.is_maximized()
                })) {
                    let mut wrapped = Vec::with_capacity(msg.len() + 1);
                    wrapped.push(MSG_TYPE_DATA);
                    wrapped.extend(msg);
                    self.callback.call((window_id.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                }

                info!("Window created successfully: {}", window_id);
            },
            EngineCommand::Quit => {
                info!("Rust: [Teardown] Quit command received. Starting 1s safety watchdog.");
                // Start a watchdog immediately in case the event loop hangs during teardown
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                    eprintln!("Rust: [Teardown] Watchdog timeout. Forcing fast exit.");
                    fast_exit(0);
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
                    let _ = instance.window.request_inner_size(size);
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
                self.winit_id_to_uuid.retain(|_, v| v != &window_id);
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

                let mut close_msg = Vec::new();
                if rmp_serde::encode::write(&mut close_msg, &serde_json::json!({"event": "window-closed", "window_id": window_id})).is_ok() {
                    let mut wrapped = Vec::with_capacity(close_msg.len() + 1);
                    wrapped.push(MSG_TYPE_DATA);
                    wrapped.extend(close_msg);
                    self.callback.call((window_id.clone(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
                }

                if self.windows.is_empty() {
                    info!("Rust: Last window closed via CloseWindow command, exiting event loop");
                    std::thread::spawn(|| {
                        std::thread::sleep(std::time::Duration::from_millis(1000));
                        fast_exit(0);
                    });
                    event_loop.exit();
                }
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
                    instance.window.request_redraw();
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
                        
                        // Unlock BEFORE painting to allow Servo to begin preparing the NEXT frame
                        pane.ready_to_repaint.store(true, Ordering::SeqCst);
                        
                        // Accept the frame Servo just spent time rendering!
                        pane.current_servo_size = pane.requested_servo_size;
                        pane.ready_frame_size = Some(pane.current_servo_size);
                        pane.needs_repaint = true;
                        
                        let was_first_frame = !pane.first_frame_painted;
                        if was_first_frame {
                            pane.first_frame_painted_time = Some(std::time::Instant::now());
                        }
                        pane.first_frame_painted = true;
                        pane.is_dirty = false;

                        // LOCK STEP: Do not dispatch webview.resize here!
                        // RedrawRequested will paint this frame into the GPU FBO, flip layout, and present.
                        // Only after presentation is complete will the next queued resize step be dispatched.
                        instance.window.request_redraw();

                        // AGGREGATION FIX: Only emit when all visible panes have painted
                        if !instance.emitted_ready_to_show {
                                let all_ready = instance.panes.values().all(|p| !p.pending_visible || p.first_frame_painted);
                                
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
            EngineCommand::CreatePane(window_id, pane_id, url, rect, z_index, anchor, dock_order, visible) => {
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

                    let origins_json = serde_json::to_string(&instance.trusted_origins)
                        .unwrap_or_else(|_| "[\"lotus-resource://localhost\"]".to_string());

                    let port_script = format!(
                        r#"(function() {{
                            const trusted = {};
                            const loc = window.location;
                            const effectiveOrigin = (loc.origin && loc.origin !== 'null') ? loc.origin : (loc.protocol + '//' + (loc.host || 'localhost'));
                            if (trusted.indexOf(effectiveOrigin) !== -1 || (loc.protocol === 'lotus-resource:' && trusted.indexOf('lotus-resource://localhost') !== -1)) {{
                                window.lotus = window.lotus || {{}};
                                window.lotus.port = {};
                                window.lotus.token = '{}';
                                window.lotus.id = '{}';
                                window.lotus.paneId = '{}';
                            }}
                        }})();"#,
                        origins_json, port, token, window_id, pane_id
                    );
                    user_content_manager.add_script(Rc::new(UserScript::from(port_script.as_str())));
                    user_content_manager.add_script(Rc::new(UserScript::from(DRAG_REGION_SCRIPT)));

                    if !pane_id.starts_with("__lotus_") && instance.scrollbar_config.enabled {
                        let cfg_script = format!(
                            "window.__LOTUS_SCROLLBAR_CONFIG__ = {{ theme: '{}', autoHide: {}, width: {} }};",
                            instance.scrollbar_config.theme,
                            instance.scrollbar_config.auto_hide,
                            instance.scrollbar_config.width
                        );
                        user_content_manager.add_script(Rc::new(UserScript::from(cfg_script.as_str())));
                        user_content_manager.add_script(Rc::new(UserScript::from(SCROLLBAR_SCRIPT)));
                    }

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
                        pending_visible: visible,
                        ghost_tex: None,
                        ghost_tex_size: None,
                        is_remote: false,
                        remote_consumer: None,
                    };
                    instance.panes.insert(pane_id, pane);
                    let physical_size = instance.window.inner_size();
                    let scale_factor = instance.window.scale_factor() as f32;
                    instance.recalculate_layout(physical_size, scale_factor);
                    instance.window.request_redraw();
                }
            },
            EngineCommand::CreateRemotePane {
                window_id,
                pane_id,
                ro_fd,
                shm_size,
                ack_rw_fd,
                rect,
                z_index,
                anchor,
                dock_order,
                visible,
            } => {
                let servo = self.ensure_servo().clone();
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if pane_id == "main" {
                        instance.auto_resize_main = false;
                    }

                    let hidpi_scale_factor_val = instance.window.scale_factor() as f32;
                    let width = (rect.size.width * hidpi_scale_factor_val).round() as u32;
                    let height = (rect.size.height * hidpi_scale_factor_val).round() as u32;
                    let servo_size = ServoPhysicalSize::new(width.max(1), height.max(1));
                    let offscreen_ctx = Rc::new(instance.rendering_context.offscreen_context(servo_size));

                    let consumer = match RemotePaneConsumer::new(ro_fd, shm_size, ack_rw_fd, width.max(1), height.max(1)) {
                        Ok(c) => Some(c),
                        Err(e) => {
                            error!("Rust: Failed to create RemotePaneConsumer for pane '{}': {:?}", pane_id, e);
                            None
                        }
                    };

                    let dummy_webview = WebViewBuilder::new(&servo, offscreen_ctx.clone()).build();

                    let pane = PaneInstance {
                        id: pane_id.clone(),
                        webview: dummy_webview,
                        rect,
                        last_notified_rect: None,
                        last_physical_rect: euclid::default::Rect::new(
                            euclid::default::Point2D::new(0, 0),
                            euclid::default::Size2D::new(servo_size.width as i32, servo_size.height as i32),
                        ),
                        z_index,
                        anchor,
                        dock_order,
                        animating: false,
                        is_dirty: true,
                        needs_repaint: true,
                        is_visible: visible,
                        drag_regions: Vec::new(),
                        no_drag_regions: Vec::new(),
                        offscreen_ctx,
                        servo_busy: false,
                        pending_servo_size: None,
                        requested_servo_size: servo_size,
                        current_servo_size: servo_size,
                        ready_to_repaint: Arc::new(AtomicBool::new(true)),
                        ready_frame_size: None,
                        first_frame_painted: true,
                        first_frame_painted_time: None,
                        pending_physical_rect: None,
                        is_resizing: false,
                        pending_visible: visible,
                        ghost_tex: None,
                        ghost_tex_size: None,
                        is_remote: true,
                        remote_consumer: consumer,
                    };
                    instance.panes.insert(pane_id, pane);
                    let physical_size = instance.window.inner_size();
                    let scale_factor = instance.window.scale_factor() as f32;
                    instance.recalculate_layout(physical_size, scale_factor);
                    instance.window.request_redraw();
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
                            if size_changed {
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
            EngineCommand::ShowSelectControl { window_id, pane_id, prompt } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    // If an existing select control was open, submit or dismiss it
                    if let Some(mut prev_active) = instance.active_select.take() {
                        instance.active_pane_id = prev_active.previous_active_pane_id;
                        if prev_active.allow_multiple {
                            prev_active.prompt.submit();
                        } else {
                            prev_active.prompt.select(prev_active.initial_selected_options.clone());
                            prev_active.prompt.submit();
                        }
                    }

                    let scale_factor = instance.window.scale_factor() as f32;
                    let (pane_px, pane_py) = if let Some(pane) = instance.panes.get(&pane_id) {
                        if pane.anchor != PaneAnchor::None {
                            (pane.last_physical_rect.origin.x as f32, pane.last_physical_rect.origin.y as f32)
                        } else {
                            let physical_rect = pane.rect.scale(scale_factor, scale_factor);
                            (physical_rect.origin.x.round(), physical_rect.origin.y.round())
                        }
                    } else {
                        (0.0, 0.0)
                    };

                    let control_id = prompt.0.id();
                    let control_id_str = format!("{:?}", control_id);
                    let pos = prompt.0.position();
                    let window_phys_x = pane_px + pos.min.x as f32;
                    let window_phys_y = pane_py + pos.min.y as f32;
                    let window_phys_w = pos.width() as f32;
                    let window_phys_h = pos.height() as f32;

                    let logical_x = window_phys_x / scale_factor;
                    let logical_y = window_phys_y / scale_factor;
                    let logical_w = window_phys_w / scale_factor;
                    let logical_h = window_phys_h / scale_factor;

                    let formatted_options: Vec<serde_json::Value> = prompt.0.options().iter().map(|item| {
                        match item {
                            servo::SelectElementOptionOrOptgroup::Option(opt) => {
                                serde_json::json!({
                                    "type": "option",
                                    "id": opt.id,
                                    "label": opt.label,
                                    "is_disabled": opt.is_disabled,
                                })
                            },
                            servo::SelectElementOptionOrOptgroup::Optgroup { label, options } => {
                                let opts: Vec<serde_json::Value> = options.iter().map(|opt| {
                                    serde_json::json!({
                                        "id": opt.id,
                                        "label": opt.label,
                                        "is_disabled": opt.is_disabled,
                                    })
                                }).collect();
                                serde_json::json!({
                                    "type": "optgroup",
                                    "label": label,
                                    "options": opts,
                                })
                            }
                        }
                    }).collect();

                    let selected_options = prompt.0.selected_options();
                    let allow_multiple = prompt.0.allow_select_multiple();

                    let previous_active_pane_id = instance.active_pane_id.clone();
                    instance.active_pane_id = "__lotus_select_overlay".to_string();

                    instance.active_select = Some(ActiveSelectPrompt {
                        control_id,
                        control_id_str: control_id_str.clone(),
                        pane_id: pane_id.clone(),
                        previous_active_pane_id,
                        initial_selected_options: selected_options.clone(),
                        prompt: prompt.0,
                        allow_multiple,
                    });

                    let payload = serde_json::json!({
                        "event": "select-element-open",
                        "window_id": window_id,
                        "pane_id": pane_id,
                        "id": control_id_str,
                        "rect": {
                            "x": logical_x,
                            "y": logical_y,
                            "width": logical_w,
                            "height": logical_h
                        },
                        "options": formatted_options,
                        "selectedOptions": selected_options,
                        "allowMultiple": allow_multiple
                    });

                    if let Ok(msg) = rmp_serde::encode::to_vec(&payload) {
                        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                            let _ = proxy.send_event(EngineCommand::IpcMessage(window_id.clone(), msg));
                        }
                    }
                }
            },
            EngineCommand::HideSelectControl { window_id, control_id } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(active) = &instance.active_select {
                        if active.control_id == control_id {
                            let mut active = instance.active_select.take().unwrap();
                            instance.active_pane_id = active.previous_active_pane_id;
                            if active.allow_multiple {
                                active.prompt.submit();
                            } else {
                                active.prompt.select(active.initial_selected_options.clone());
                                active.prompt.submit();
                            }
                            if let Some(overlay) = instance.panes.get_mut("__lotus_select_overlay") {
                                overlay.pending_visible = false;
                                overlay.is_dirty = true;
                                let physical_size = instance.window.inner_size();
                                let scale_factor = instance.window.scale_factor() as f32;
                                instance.recalculate_layout(physical_size, scale_factor);
                                instance.window.request_redraw();
                            }
                            if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                                "event": "select-element-close",
                                "window_id": window_id,
                            })) {
                                if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                                    let _ = proxy.send_event(EngineCommand::IpcMessage(window_id.clone(), msg));
                                }
                            }
                        }
                    }
                }
            },
            EngineCommand::UpdateSelectControl { window_id, control_id, selected_indices } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(active) = &mut instance.active_select {
                        if active.control_id_str == control_id || control_id.is_empty() {
                            let indices: Vec<usize> = selected_indices.into_iter().map(|i| i as usize).collect();
                            active.prompt.select(indices);
                        }
                    }
                }
            },
            EngineCommand::SubmitSelectControl { window_id, control_id: _, selected_indices } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(mut active) = instance.active_select.take() {
                        instance.active_pane_id = active.previous_active_pane_id;
                        let indices: Vec<usize> = selected_indices.into_iter().map(|i| i as usize).collect();
                        active.prompt.select(indices);
                        active.prompt.submit();
                    }
                    if let Some(overlay) = instance.panes.get_mut("__lotus_select_overlay") {
                        overlay.pending_visible = false;
                        overlay.is_dirty = true;
                        let physical_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;
                        instance.recalculate_layout(physical_size, scale_factor);
                        instance.window.request_redraw();
                    }
                    if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                        "event": "select-element-close",
                        "window_id": window_id,
                    })) {
                        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                            let _ = proxy.send_event(EngineCommand::IpcMessage(window_id.clone(), msg));
                        }
                    }
                }
            },
            EngineCommand::DismissSelectControl { window_id, control_id: _ } => {
                if let Some(instance) = self.windows.get_mut(&window_id) {
                    if let Some(mut active) = instance.active_select.take() {
                        instance.active_pane_id = active.previous_active_pane_id;
                        active.prompt.select(active.initial_selected_options.clone());
                        active.prompt.submit();
                    }
                    if let Some(overlay) = instance.panes.get_mut("__lotus_select_overlay") {
                        overlay.pending_visible = false;
                        overlay.is_dirty = true;
                        let physical_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;
                        instance.recalculate_layout(physical_size, scale_factor);
                        instance.window.request_redraw();
                    }
                    if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                        "event": "select-element-close",
                        "window_id": window_id,
                    })) {
                        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                            let _ = proxy.send_event(EngineCommand::IpcMessage(window_id.clone(), msg));
                        }
                    }
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

                    let mut theme_msg = Vec::new();
                    if rmp_serde::encode::write(&mut theme_msg, &serde_json::json!({
                        "event": "theme-changed",
                        "theme": theme_str
                    })).is_ok() {
                        let mut wrapped = Vec::with_capacity(theme_msg.len() + 1);
                        wrapped.push(MSG_TYPE_DATA);
                        wrapped.extend(theme_msg);
                        self.callback.call(("global".to_string(), vec![wrapped.into()]), ThreadsafeFunctionCallMode::NonBlocking);
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
                        let sf = scale_factor as f32;
                        for pane in instance.panes.values_mut() {
                            pane.webview.set_hidpi_scale_factor(euclid::Scale::new(sf));
                        }
                        instance.recalculate_layout(instance.active_window_size, sf);
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
                            std::thread::spawn(|| {
                                std::thread::sleep(std::time::Duration::from_millis(1000));
                                fast_exit(0);
                            });
                            event_loop.exit();
                        }
                    },
                    WindowEvent::RedrawRequested => {
                        let _ = instance.rendering_context.make_current();
                        let window_size = instance.window.inner_size();
                        let scale_factor = instance.window.scale_factor() as f32;

                        trace!("Rust: [Lifecycle] RedrawRequested (FBO) for window size {}x{}", window_size.width, window_size.height);

                        unsafe {
                            // 1. UNIVERSAL PAINT BLOCK
                            // Paint any panes that have new rendered frames ready in Servo
                            for p in instance.panes.values_mut() {
                                if p.needs_repaint {
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
                                    }
                                    p.needs_repaint = false;
                                }
                            }

                            // 2. Evaluate Integrity against the COMMITTED target
                            let is_transaction_pending = instance.committed_layout.is_some();
                            let mut transaction_complete = false;

                            if let Some(committed) = &instance.committed_layout {
                                transaction_complete = true;
                                for (id, staged_pane) in &committed.panes {
                                    if staged_pane.visible {
                                        if let Some(pane) = instance.panes.get(id) {
                                            let target_size = ServoPhysicalSize::new(
                                                (staged_pane.rect.size.width as u32).max(1), 
                                                (staged_pane.rect.size.height as u32).max(1)
                                            );
                                            let current_ready = pane.ready_frame_size.unwrap_or(ServoPhysicalSize::new(0, 0));
                                            
                                            if current_ready.width != target_size.width || current_ready.height != target_size.height {
                                                transaction_complete = false;
                                                break;
                                            }
                                            // For the very first spawn of a pane, hold the lock for 150ms 
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
                            let mut _flip_occurred = false;

                            // 3. THE NUCLEAR GEOMETRY FLIP
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

                            let active_w = instance.active_window_size.width.max(1);
                            let active_h = instance.active_window_size.height.max(1);

                            gl::ensure_textures(instance, active_w, active_h);
                            gl::composite_panes_to_scene(
                                instance,
                                active_w,
                                active_h,
                                scale_factor,
                                is_transaction_pending,
                                transaction_complete,
                                &pending_hides,
                            );

                            let live_w = window_size.width.max(1) as i32;
                            let live_h = window_size.height.max(1) as i32;
                            gl::blit_scene_to_window(instance, live_w, live_h, scale_factor);

                            // LOCK STEP: The current frame is now fully presented to the GPU and screen.
                            if instance.committed_layout.is_none() {
                                for pane in instance.panes.values_mut() {
                                    pane.servo_busy = false;
                                }
                                let window_size = instance.window.inner_size();
                                let scale_factor = instance.window.scale_factor() as f32;
                                instance.recalculate_layout(window_size, scale_factor);
                            }

                            trace!("Rust: [Lifecycle] RedrawRequested DONE");
                        }
                    },            WindowEvent::Resized(size) => {
                let scale_factor = instance.window.scale_factor() as f32;

                if let Some(mut active) = instance.active_select.take() {
                    instance.active_pane_id = active.previous_active_pane_id;
                    if active.allow_multiple {
                        active.prompt.submit();
                    } else {
                        active.prompt.select(active.initial_selected_options.clone());
                        active.prompt.submit();
                    }
                    if let Some(overlay) = instance.panes.get_mut("__lotus_select_overlay") {
                        overlay.pending_visible = false;
                        overlay.is_dirty = true;
                    }
                    if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                        "event": "select-element-close",
                        "window_id": uuid,
                    })) {
                        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                            let _ = proxy.send_event(EngineCommand::IpcMessage(uuid.clone(), msg));
                        }
                    }
                }
                
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
                // SYNCHRONOUS PRESENTATION ON RESIZE:
                // Instantly stretch the last known good scene quad across the entire expanded window surface.
                // This eliminates any asynchronous gap between Mutter/Xwayland surface resize and Winit RedrawRequested.
                let live_w = size.width.max(1) as i32;
                let live_h = size.height.max(1) as i32;
                gl::blit_scene_to_window(instance, live_w, live_h, scale_factor);

                instance.window.request_redraw();

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
                    "logicalHeight": logical_h,
                    "maximized": instance.window.is_maximized()
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
                                
                                // 1. In frameless windows, check window resize borders first
                                let resize_cursor = if instance.frameless {
                                    let size = instance.window.inner_size();
                                    let x = position.x;
                                    let y = position.y;
                                    let w = size.width as f64;
                                    let h = size.height as f64;
                                    let border = (6.0 * scale_factor as f64).round().max(4.0);
                                    
                                    if x < border && y < border {
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
                                    }
                                } else {
                                    None
                                };
                                
                                if let Some(icon) = resize_cursor {
                                    instance.window.set_cursor(icon);
                                    instance.in_resize_border = true;
                                } else {
                                    if instance.in_resize_border {
                                        instance.window.set_cursor(winit::window::CursorIcon::Default);
                                        instance.in_resize_border = false;
                                    }
                                    if hit_drag {
                                        instance.window.set_cursor(winit::window::CursorIcon::Move);
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

                        if is_pressed {
                            if let Some(_) = &instance.active_select {
                                let overlay_pane = instance.panes.get("__lotus_select_overlay");
                                let is_inside_overlay = overlay_pane.map(|p| {
                                    if !p.is_visible { return false; }
                                    let (px, py, pw, ph) = if p.anchor != PaneAnchor::None {
                                        (
                                            p.last_physical_rect.origin.x as f32,
                                            p.last_physical_rect.origin.y as f32,
                                            p.last_physical_rect.size.width as f32,
                                            p.last_physical_rect.size.height as f32,
                                        )
                                    } else {
                                        let physical_rect = p.rect.scale(scale_factor, scale_factor);
                                        (
                                            physical_rect.origin.x.round(),
                                            physical_rect.origin.y.round(),
                                            physical_rect.size.width.round(),
                                            physical_rect.size.height.round(),
                                        )
                                    };
                                    let rect = euclid::Rect::new(euclid::Point2D::new(px, py), euclid::Size2D::new(pw, ph));
                                    rect.contains(instance.last_mouse_pos)
                                }).unwrap_or(false);

                                if !is_inside_overlay {
                                    let mut active = instance.active_select.take().unwrap();
                                    instance.active_pane_id = active.previous_active_pane_id;
                                    if active.allow_multiple {
                                        active.prompt.submit();
                                    } else {
                                        active.prompt.select(active.initial_selected_options.clone());
                                        active.prompt.submit();
                                    }
                                    if let Some(overlay) = instance.panes.get_mut("__lotus_select_overlay") {
                                        overlay.pending_visible = false;
                                        overlay.is_dirty = true;
                                        let physical_size = instance.window.inner_size();
                                        instance.recalculate_layout(physical_size, scale_factor);
                                        instance.window.request_redraw();
                                    }
                                    if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                                        "event": "select-element-close",
                                        "window_id": uuid,
                                    })) {
                                        if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                                            let _ = proxy.send_event(EngineCommand::IpcMessage(uuid.clone(), msg));
                                        }
                                    }
                                    return;
                                }
                            }
                        }
                        
                        let action = match state {
                            winit::event::ElementState::Pressed => MouseButtonAction::Down,
                            winit::event::ElementState::Released => MouseButtonAction::Up,
                        };
                        let servo_button = match button {
                            winit::event::MouseButton::Left => ServoMouseButton::Primary,
                            winit::event::MouseButton::Right => ServoMouseButton::Secondary,
                            winit::event::MouseButton::Middle => ServoMouseButton::Auxiliary,
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
                                        let size = instance.window.inner_size();
                                        let x = instance.last_mouse_pos.x as f64;
                                        let y = instance.last_mouse_pos.y as f64;
                                        let w = size.width as f64;
                                        let h = size.height as f64;
                                        let border = (6.0 * scale_factor as f64).round().max(4.0);
                                        
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
                        let scale_factor = instance.window.scale_factor() as f32;
                        // Standard browsers (Chromium/Firefox) scroll ~100 CSS px per wheel notch (approx. 4-5 text lines).
                        // Scale by scale_factor so HiDPI displays maintain the intended CSS scroll distance.
                        // Precision touchpads and smooth wheels emit PixelDelta, which is already in physical pixels.
                        let line_scroll = 100.0 * scale_factor as f64;
                        let (x, y) = match delta {
                            MouseScrollDelta::LineDelta(x, y) => (x as f64 * line_scroll, y as f64 * line_scroll),
                            MouseScrollDelta::PixelDelta(pos) => (pos.x, pos.y),
                        };
                        let wheel_delta = WheelDelta {
                            x,
                            y,
                            z: 0.0,
                            mode: WheelMode::DeltaPixel,
                        };

                        if let Some(_) = &instance.active_select {
                            let overlay_pane = instance.panes.get("__lotus_select_overlay");
                            let is_inside_overlay = overlay_pane.map(|p| {
                                if !p.is_visible { return false; }
                                let (px, py, pw, ph) = if p.anchor != PaneAnchor::None {
                                    (
                                        p.last_physical_rect.origin.x as f32,
                                        p.last_physical_rect.origin.y as f32,
                                        p.last_physical_rect.size.width as f32,
                                        p.last_physical_rect.size.height as f32,
                                    )
                                } else {
                                    let physical_rect = p.rect.scale(scale_factor, scale_factor);
                                    (
                                        physical_rect.origin.x.round(),
                                        physical_rect.origin.y.round(),
                                        physical_rect.size.width.round(),
                                        physical_rect.size.height.round(),
                                    )
                                };
                                let rect = euclid::Rect::new(euclid::Point2D::new(px, py), euclid::Size2D::new(pw, ph));
                                rect.contains(instance.last_mouse_pos)
                            }).unwrap_or(false);

                            if !is_inside_overlay {
                                let mut active = instance.active_select.take().unwrap();
                                instance.active_pane_id = active.previous_active_pane_id;
                                if active.allow_multiple {
                                    active.prompt.submit();
                                } else {
                                    active.prompt.select(active.initial_selected_options.clone());
                                    active.prompt.submit();
                                }
                                if let Some(overlay) = instance.panes.get_mut("__lotus_select_overlay") {
                                    overlay.pending_visible = false;
                                    overlay.is_dirty = true;
                                    let physical_size = instance.window.inner_size();
                                    instance.recalculate_layout(physical_size, scale_factor);
                                    instance.window.request_redraw();
                                }
                                if let Ok(msg) = rmp_serde::encode::to_vec(&serde_json::json!({
                                    "event": "select-element-close",
                                    "window_id": uuid,
                                })) {
                                    if let Some(proxy) = EVENT_LOOP_PROXY.get() {
                                        let _ = proxy.send_event(EngineCommand::IpcMessage(uuid.clone(), msg));
                                    }
                                }
                            }
                        }

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


