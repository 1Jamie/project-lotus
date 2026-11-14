//! Host-side resize grips for floating (`Anchor::None`) overlays — hit test only, no chrome.

use euclid::{Point2D, Rect, Size2D};
use servo::DeviceIndependentPixel;
use winit::window::CursorIcon;

use crate::layout::PaneAnchor;

/// Bitmask: left=1, right=2, top=4, bottom=8
pub type ResizeEdgeMask = u32;

pub const RESIZE_LEFT: ResizeEdgeMask = 1;
pub const RESIZE_RIGHT: ResizeEdgeMask = 2;
pub const RESIZE_TOP: ResizeEdgeMask = 4;
pub const RESIZE_BOTTOM: ResizeEdgeMask = 8;
pub const RESIZE_ALL: ResizeEdgeMask = RESIZE_LEFT | RESIZE_RIGHT | RESIZE_TOP | RESIZE_BOTTOM;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeAnchor {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl ResizeAnchor {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => ResizeAnchor::TopRight,
            2 => ResizeAnchor::BottomLeft,
            3 => ResizeAnchor::BottomRight,
            _ => ResizeAnchor::TopLeft,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveResizeEdge {
    Left,
    Right,
    Top,
    Bottom,
    BottomLeft,
    BottomRight,
    TopLeft,
    TopRight,
}

#[derive(Clone, Debug)]
pub struct OverlayResizePolicy {
    pub edges: ResizeEdgeMask,
    pub anchor: ResizeAnchor,
    pub min_width: f32,
    pub min_height: f32,
    pub max_width: Option<f32>,
    pub max_height: Option<f32>,
}

impl OverlayResizePolicy {
    pub fn disabled() -> Option<Self> {
        None
    }

    pub fn from_parts(
        edges: u32,
        anchor: u8,
        min_w: f32,
        min_h: f32,
        max_w: Option<f32>,
        max_h: Option<f32>,
    ) -> Option<Self> {
        if edges == 0 {
            return None;
        }
        Some(Self {
            edges,
            anchor: ResizeAnchor::from_u8(anchor),
            min_width: min_w.max(32.0),
            min_height: min_h.max(32.0),
            max_width: max_w.filter(|v| *v > 0.0),
            max_height: max_h.filter(|v| *v > 0.0),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ActiveOverlayResizeDrag {
    pub pane_id: String,
    pub edge: ActiveResizeEdge,
    pub start_rect: Rect<f32, DeviceIndependentPixel>,
}

pub fn resize_border_physical(scale_factor: f32) -> f32 {
    (6.0_f32 * scale_factor).round().max(4.0)
}

pub fn cursor_for_edge(edge: ActiveResizeEdge) -> CursorIcon {
    match edge {
        ActiveResizeEdge::Left | ActiveResizeEdge::Right => CursorIcon::ColResize,
        ActiveResizeEdge::Top | ActiveResizeEdge::Bottom => CursorIcon::RowResize,
        ActiveResizeEdge::BottomLeft | ActiveResizeEdge::TopRight => CursorIcon::NeswResize,
        ActiveResizeEdge::BottomRight | ActiveResizeEdge::TopLeft => CursorIcon::NwseResize,
    }
}

/// Hit-test floating overlay resize strips (physical coordinates).
pub fn hit_overlay_resize_edge(
    policy: &OverlayResizePolicy,
    px: f32,
    py: f32,
    pw: f32,
    ph: f32,
    point: Point2D<f32, servo::DevicePixel>,
    border: f32,
) -> Option<ActiveResizeEdge> {
    if pw < 1.0 || ph < 1.0 {
        return None;
    }
    let x = point.x;
    let y = point.y;
    if x < px - 0.5 || y < py - 0.5 || x > px + pw + 0.5 || y > py + ph + 0.5 {
        return None;
    }
    let in_left = policy.edges & RESIZE_LEFT != 0 && x < px + border;
    let in_right = policy.edges & RESIZE_RIGHT != 0 && x > px + pw - border;
    let in_top = policy.edges & RESIZE_TOP != 0 && y < py + border;
    let in_bottom = policy.edges & RESIZE_BOTTOM != 0 && y > py + ph - border;

    if in_left && in_bottom {
        return Some(ActiveResizeEdge::BottomLeft);
    }
    if in_right && in_bottom {
        return Some(ActiveResizeEdge::BottomRight);
    }
    if in_left && in_top {
        return Some(ActiveResizeEdge::TopLeft);
    }
    if in_right && in_top {
        return Some(ActiveResizeEdge::TopRight);
    }
    if in_left {
        return Some(ActiveResizeEdge::Left);
    }
    if in_right {
        return Some(ActiveResizeEdge::Right);
    }
    if in_top {
        return Some(ActiveResizeEdge::Top);
    }
    if in_bottom {
        return Some(ActiveResizeEdge::Bottom);
    }
    None
}

pub fn compute_resized_rect(
    policy: &OverlayResizePolicy,
    drag: &ActiveOverlayResizeDrag,
    cursor_x: f32,
    cursor_y: f32,
    window_w: f32,
    window_h: f32,
) -> Rect<f32, DeviceIndependentPixel> {
    let mut r = drag.start_rect;
    let min_w = policy.min_width;
    let min_h = policy.min_height;
    let max_w = policy.max_width.unwrap_or(window_w);
    let max_h = policy.max_height.unwrap_or(window_h);

    match policy.anchor {
        ResizeAnchor::TopLeft => {
            match drag.edge {
                ActiveResizeEdge::Left => {
                    let right = r.origin.x + r.size.width;
                    let new_x = cursor_x.clamp((right - max_w).max(0.0), right - min_w);
                    r.size.width = (right - new_x).clamp(min_w, max_w);
                    r.origin.x = right - r.size.width;
                }
                ActiveResizeEdge::Right => {
                    let new_w = (cursor_x - r.origin.x).clamp(min_w, max_w);
                    r.size.width = new_w.min(window_w - r.origin.x);
                }
                ActiveResizeEdge::Top => {
                    let bottom = r.origin.y + r.size.height;
                    let new_y = cursor_y.clamp((bottom - max_h).max(0.0), bottom - min_h);
                    r.size.height = (bottom - new_y).clamp(min_h, max_h);
                    r.origin.y = bottom - r.size.height;
                }
                ActiveResizeEdge::Bottom => {
                    let new_h = (cursor_y - r.origin.y).clamp(min_h, max_h);
                    r.size.height = new_h.min(window_h - r.origin.y);
                }
                _ => {}
            }
        }
        ResizeAnchor::TopRight => {
            let fixed_right = drag.start_rect.origin.x + drag.start_rect.size.width;
            let fixed_top = drag.start_rect.origin.y;
            match drag.edge {
                ActiveResizeEdge::BottomLeft => {
                    let new_w = (fixed_right - cursor_x).clamp(min_w, max_w);
                    r.origin.x = fixed_right - new_w;
                    r.size.width = new_w;
                    let new_h = (cursor_y - fixed_top).clamp(min_h, max_h);
                    r.origin.y = fixed_top;
                    r.size.height = new_h.min(window_h - fixed_top);
                }
                ActiveResizeEdge::Left | ActiveResizeEdge::TopRight | ActiveResizeEdge::TopLeft => {
                    let new_w = (fixed_right - cursor_x).clamp(min_w, max_w);
                    r.origin.x = fixed_right - new_w;
                    r.size.width = new_w;
                }
                ActiveResizeEdge::Right | ActiveResizeEdge::BottomRight => {
                    let new_w = (fixed_right - drag.start_rect.origin.x).clamp(min_w, max_w);
                    r.origin.x = fixed_right - new_w;
                    r.size.width = new_w;
                }
                ActiveResizeEdge::Top => {
                    let bottom = drag.start_rect.origin.y + drag.start_rect.size.height;
                    let new_y = cursor_y.clamp((bottom - max_h).max(0.0), bottom - min_h);
                    r.size.height = (bottom - new_y).clamp(min_h, max_h);
                    r.origin.y = bottom - r.size.height;
                }
                ActiveResizeEdge::Bottom => {
                    let new_h = (cursor_y - fixed_top).clamp(min_h, max_h);
                    r.origin.y = fixed_top;
                    r.size.height = new_h.min(window_h - fixed_top);
                }
                _ => {}
            }
        }
        ResizeAnchor::BottomLeft | ResizeAnchor::BottomRight => {
            // Rare for atrium; minimal support using top-left math as fallback.
            let _ = (window_w, window_h);
            match drag.edge {
                ActiveResizeEdge::Left => {
                    let right = r.origin.x + r.size.width;
                    let new_x = cursor_x.clamp(0.0, right - min_w);
                    r.size.width = (right - new_x).clamp(min_w, max_w);
                    r.origin.x = right - r.size.width;
                }
                ActiveResizeEdge::Bottom => {
                    let new_h = (cursor_y - r.origin.y).clamp(min_h, max_h);
                    r.size.height = new_h;
                }
                _ => {}
            }
        }
    }

    r.origin.x = r.origin.x.clamp(0.0, (window_w - r.size.width).max(0.0));
    r.origin.y = r.origin.y.clamp(0.0, (window_h - r.size.height).max(0.0));
    r.size.width = r.size.width.clamp(min_w, max_w.min(window_w - r.origin.x));
    r.size.height = r.size.height
        .clamp(min_h, max_h.min(window_h - r.origin.y));
    r
}

/// Only floating overlays with an enabled policy.
pub fn pane_eligible_for_overlay_resize(anchor: PaneAnchor, policy: &Option<OverlayResizePolicy>) -> bool {
    anchor == PaneAnchor::None && policy.is_some()
}

/// `pane_physical_rect` must match compositor placement (staged layout + contain-fit).
pub fn hit_test_overlay_resize<'a, I>(
    panes: I,
    point: Point2D<f32, servo::DevicePixel>,
    scale_factor: f32,
    pointer_ok: impl Fn(&crate::window::PaneInstance) -> bool,
    pane_physical_rect: impl Fn(&crate::window::PaneInstance) -> (f32, f32, f32, f32),
) -> Option<(String, ActiveResizeEdge)>
where
    I: Iterator<Item = &'a crate::window::PaneInstance>,
{
    let border = resize_border_physical(scale_factor);
    let mut sorted: Vec<_> = panes.collect();
    sorted.sort_by_key(|p| -p.z_index);
    for pane in sorted {
        if !pointer_ok(pane) {
            continue;
        }
        if !pane_eligible_for_overlay_resize(pane.anchor, &pane.overlay_resize) {
            continue;
        }
        let policy = pane.overlay_resize.as_ref()?;
        let (px, py, pw, ph) = pane_physical_rect(pane);
        if let Some(edge) = hit_overlay_resize_edge(policy, px, py, pw, ph, point, border) {
            return Some((pane.id.clone(), edge));
        }
    }
    None
}

/// Contain-fit content quad in physical space (same math as compositor external draw).
pub fn contain_fit_content_rect(
    px: f32,
    py: f32,
    pw: f32,
    ph: f32,
    buf_w: u32,
    buf_h: u32,
) -> (f32, f32, f32, f32) {
    let bw = buf_w.max(1) as f32;
    let bh = buf_h.max(1) as f32;
    if pw < 1.0 || ph < 1.0 {
        return (px, py, pw, ph);
    }
    let pane_ar = pw / ph;
    let buf_ar = bw / bh;
    if (pane_ar - buf_ar).abs() < 0.02 {
        return (px, py, pw, ph);
    }
    if buf_ar > pane_ar {
        let dh = (pw / buf_ar).round().max(1.0);
        let dy = py + (ph - dh) * 0.5;
        (px, dy, pw, dh)
    } else {
        let dw = (ph * buf_ar).round().max(1.0);
        let dx = px + (pw - dw) * 0.5;
        (dx, py, dw, ph)
    }
}

