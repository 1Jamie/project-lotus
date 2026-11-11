'use strict';

/**
 * Live atrium window bounds in logical pixels (overlay layout space).
 */
function windowBounds(win, fallbackW = 1280, fallbackH = 720) {
  // Prefer live host logical size; fall back to last known game surface
  // (HUD SURFACE WxH) so drawers track 1600×900 vs 1920×1080 dynamically.
  const w = Math.max(
    1,
    Math.floor(
      (win &&
        (win._logicalWidth ||
          win._gameSurface?.width ||
          win._layout?.width)) ||
        fallbackW
    )
  );
  const h = Math.max(
    1,
    Math.floor(
      (win &&
        (win._logicalHeight ||
          win._gameSurface?.height ||
          win._layout?.height)) ||
        fallbackH
    )
  );
  return { x: 0, y: 0, width: w, height: h };
}

function rectFits(rect, bounds, margin = 0) {
  if (!rect || !bounds) return false;
  const x = Number(rect.x);
  const y = Number(rect.y);
  const w = Number(rect.width);
  const h = Number(rect.height);
  if (![x, y, w, h].every((n) => Number.isFinite(n) && n >= 0)) return false;
  if (w < 32 || h < 32) return false;
  if (x < bounds.x - margin) return false;
  if (y < bounds.y - margin) return false;
  if (x + w > bounds.x + bounds.width + margin) return false;
  if (y + h > bounds.y + bounds.height + margin) return false;
  return true;
}

/**
 * Clamp a rect into bounds. Shrinks width/height if needed, then shifts x/y.
 */
function clampRect(rect, bounds) {
  const bw = Math.max(1, bounds.width);
  const bh = Math.max(1, bounds.height);
  let w = Math.max(1, Math.min(Math.floor(Number(rect.width) || 1), bw));
  let h = Math.max(1, Math.min(Math.floor(Number(rect.height) || 1), bh));
  let x = Math.floor(Number(rect.x) || 0);
  let y = Math.floor(Number(rect.y) || 0);
  if (x + w > bounds.x + bw) x = bounds.x + bw - w;
  if (y + h > bounds.y + bh) y = bounds.y + bh - h;
  if (x < bounds.x) x = bounds.x;
  if (y < bounds.y) y = bounds.y;
  return { x, y, width: w, height: h };
}

/**
 * Right-side drawer (Discord): scale to the current window, never force a
 * minimum that overflows a small surface (e.g. 1600×900 after 1920×1080).
 */
function rightDrawerRect(win, opts = {}) {
  const top = opts.top != null ? opts.top : 56;
  const maxWidth = opts.maxWidth != null ? opts.maxWidth : 900;
  const fraction = opts.fraction != null ? opts.fraction : 0.38;
  const minWidth = opts.minWidth != null ? opts.minWidth : 280;
  const bounds = windowBounds(win, opts.fallbackW, opts.fallbackH);
  const availW = Math.max(1, bounds.width);
  const availH = Math.max(1, bounds.height - top);
  const preferred = Math.floor(availW * fraction);
  const width = Math.max(
    Math.min(minWidth, availW),
    Math.min(maxWidth, preferred, availW)
  );
  return {
    x: Math.max(0, availW - width),
    y: Math.min(top, Math.max(0, bounds.height - 1)),
    width,
    height: Math.max(1, availH),
  };
}

module.exports = {
  windowBounds,
  rectFits,
  clampRect,
  rightDrawerRect,
};
