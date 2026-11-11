'use strict';

async function activate(ctx) {
  // Surfaces come from plugin.json via PluginManager auto-mount.
  ctx.telemetry.onFrameStats((stats) => {
    ctx.ipc.send('stats', stats);
  });
  ctx.log('perf-hud active');
}

async function deactivate() {}

module.exports = { activate, deactivate };
