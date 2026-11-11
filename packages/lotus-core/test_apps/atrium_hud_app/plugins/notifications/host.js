'use strict';

const { clampRect, windowBounds } = require('../../../../lib/atrium/bounds');

const TOAST_W = 380;
const TOAST_H = 320;
const TOP = 64;
const MARGIN = 16;

function toastRect(win) {
  const b = windowBounds(win);
  const width = Math.min(TOAST_W, Math.max(200, b.width - MARGIN * 2));
  const height = Math.min(TOAST_H, Math.max(100, b.height - TOP - MARGIN));
  return clampRect(
    {
      x: Math.max(MARGIN, b.width - width - MARGIN),
      y: TOP,
      width,
      height,
    },
    b
  );
}

async function activate(ctx) {
  const win = ctx.shell.win;
  const r = toastRect(win);

  await ctx.createSurfacePane('toasts', {
    entry: 'ui/index.html',
    defaultRect: r,
    layer: 'shell',
    zIndex: 980,
    visibility: 'always',
    // Passive-only over the game; overlay mode can later flip interactive.
    interactive: 'never',
    reposition(pane, w) {
      const next = toastRect(w);
      pane.setRect(next.x, next.y, next.width, next.height);
    },
  });

  const onNotify = (payload) => {
    // Quiet game-mode filter: drop low urgency while playing.
    if (ctx.shell.mode === 'game' && payload && payload.urgency === 'low') {
      return;
    }
    ctx.ipc.send('toast', payload);
  };

  const onClosed = (payload) => {
    ctx.ipc.send('toast-closed', payload || {});
  };

  const onAction = (payload) => {
    ctx.ipc.send('toast-action', payload || {});
  };

  ctx.shell.on('notify', onNotify);
  ctx.shell.on('notify-closed', onClosed);
  ctx.shell.on('notify-action', onAction);
  ctx._offNotify = () => {
    ctx.shell.off('notify', onNotify);
    ctx.shell.off('notify-closed', onClosed);
    ctx.shell.off('notify-action', onAction);
  };

  ctx.log('notifications toast surface active');
}

async function deactivate(ctx) {
  if (ctx && typeof ctx._offNotify === 'function') ctx._offNotify();
}

module.exports = { activate, deactivate };
