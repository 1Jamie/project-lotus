'use strict';

const { rightDrawerRect } = require('../../../../lib/atrium/bounds');

const TOP_CHROME = 56;

function discordRect(win) {
  // Bound to the live atrium window — never keep a 720px min that overflows
  // 1600×900 (or any size smaller than a prior 1920×1080 profile rect).
  return rightDrawerRect(win, {
    top: TOP_CHROME,
    maxWidth: 900,
    fraction: 0.38,
    minWidth: 280,
  });
}

async function activate(ctx) {
  const win = ctx.shell.win;
  const r = discordRect(win);

  await ctx.createExternalPane('app', {
    // Legacy id so Rust F9 bridge (pane "discord") still works.
    rawPaneId: 'discord',
    command: 'flatpak',
    args: [
      'run',
      'com.discordapp.Discord',
      '--multi-instance',
      '--no-sandbox',
      '--enable-features=UseOzonePlatform',
      '--ozone-platform=wayland',
    ],
    defaultRect: r,
    layer: 'drawer',
    visibility: 'overlay-only',
    interactive: 'overlay-only',
    // Host-bus Notify → atrium toasts while Discord stays on the real session bus.
    dbus: {
      notifications: 'mirror',
      notificationApps: ['Discord', 'discord', 'com.discordapp.Discord'],
    },
    reposition(p, w) {
      const next = discordRect(w);
      p.setRect(next.x, next.y, next.width, next.height);
    },
  });

  ctx.log('discord-sidecar active (pane id=discord for F9 bridge)');
}

async function deactivate() {}

module.exports = { activate, deactivate };
