'use strict';

const SIZE = 64;

function centerRect(win) {
  const w = win._logicalWidth || 1280;
  const h = win._logicalHeight || 720;
  return {
    x: Math.max(0, Math.floor((w - SIZE) / 2)),
    y: Math.max(0, Math.floor((h - SIZE) / 2)),
    width: SIZE,
    height: SIZE,
  };
}

async function activate(ctx) {
  const win = ctx.shell.win;
  const r = win ? centerRect(win) : { x: 608, y: 328, width: SIZE, height: SIZE };
  await ctx.createSurfacePane('main', {
    entry: 'ui/index.html',
    defaultRect: r,
    layer: 'hud',
    visibility: 'always',
    interactive: 'never',
    reposition(pane, w) {
      const next = centerRect(w);
      pane.setRect(next.x, next.y, next.width, next.height);
    },
  });
  ctx.log('static-crosshair active');
}

async function deactivate() {}

module.exports = { activate, deactivate };
