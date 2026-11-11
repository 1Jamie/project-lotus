'use strict';

const path = require('path');
const { EventEmitter } = require('events');
const { ServoWindow, Anchor, ipcMain } = require('../../lotus.js');
const {
  TOP_CHROME,
  ensureConfigLayout,
  loadProfile,
  saveProfile,
  pluginsDir,
  resolveKeybinds,
} = require('./profile');
const { loadPlugins } = require('./plugin_manager');
const { zIndexForLayer } = require('./layers');

/**
 * Start the Atrium shell: compact top-bar chrome, game pane, plugin host.
 */
async function startShell(cfg) {
  const bundledPluginsDir = path.join(__dirname, '../../test_apps/atrium_hud_app/plugins');
  const layout = ensureConfigLayout(bundledPluginsDir);
  const profile = loadProfile();

  const gameLabel = cfg.gameLabel || 'game';
  const shellUiDir = path.join(__dirname, '../../test_apps/atrium_hud_app/ui');

  const shellZ = zIndexForLayer('shell');
  const winW = cfg.width || 1280;
  const winH = cfg.height || 720;

  const win = new ServoWindow({
    id: 'atrium-hud-window',
    root: shellUiDir,
    index: 'shell.html',
    width: winW,
    height: winH,
    title: `Atrium — ${gameLabel}`,
    wmClass: 'lotus-atrium',
    frameless: cfg.gameMode,
    transparent: !cfg.gameMode,
    visible: false,
    // Floating top strip (Anchor.None) — NOT docked. A docked Top+Fill pair was
    // ending up full-window, centering the bar mid-screen and stealing Discord hits.
    panes: [
      {
        id: 'main',
        url: 'lotus-resource://localhost/shell.html',
        x: 0,
        y: 0,
        width: winW,
        height: TOP_CHROME,
        zIndex: shellZ,
        visible: true,
        anchor: Anchor.None,
        dockOrder: 0,
      },
    ],
    autoResizeMain: false,
  });

  function pinShellBar() {
    const w = win._logicalWidth || cfg.width || winW;
    try {
      // Keep chrome as a 56px hit-target at the top only.
      win.pane.setRect(0, 0, w, TOP_CHROME);
      win.pane.zIndex = shellZ;
    } catch (e) {
      console.warn('[atrium] pinShellBar failed:', e.message);
    }
  }

  /** Recompute Discord/HUD/toast panes for the current atrium resolution. */
  function refitOverlays(reason) {
    pinShellBar();
    for (const { ctx } of shellApi._plugins.values()) {
      try {
        ctx._onWindowResized();
      } catch (e) {
        console.warn('[atrium] overlay refit failed:', e.message);
      }
    }
    if (reason) {
      console.log(
        `[atrium] overlay refit (${reason}) → ${win._logicalWidth || '?'}x${win._logicalHeight || '?'}`
      );
    }
  }

  const shellApi = new EventEmitter();
  shellApi.win = win;
  shellApi.mode = cfg.startHudMode ? 'overlay' : 'game';
  shellApi._plugins = new Map();
  shellApi._telemetrySubs = [];
  shellApi._buttons = [];
  shellApi._claimedShortcuts = new Map(); // shortcut → pluginId
  shellApi._surfaceSize = null;
  shellApi.refitOverlays = refitOverlays;

  shellApi.registerButton = (btn) => {
    const id = btn.id || `btn-${shellApi._buttons.length}`;
    const shortcut = btn.shortcut || null;
    if (shortcut) {
      if (shellApi._claimedShortcuts.has(shortcut)) {
        console.warn(
          `[atrium] shortcut ${shortcut} already claimed by ${shellApi._claimedShortcuts.get(shortcut)}; ignoring ${btn.pluginId || 'shell'}`
        );
      } else {
        shellApi._claimedShortcuts.set(shortcut, btn.pluginId || 'shell');
      }
    }
    shellApi._buttons.push({ ...btn, id });
    pushShellChrome();
  };

  shellApi.setMode = (mode) => {
    const next = mode === 'edit' ? 'edit' : mode === 'overlay' ? 'overlay' : 'game';
    if (next === shellApi.mode) return;
    shellApi.mode = next;
    // game_mode Rust flag: true = game input, false = overlay/edit input
    win.setGameMode(next === 'game');
    for (const { ctx } of shellApi._plugins.values()) {
      ctx._applyShellMode(next);
    }
    shellApi.emit('mode-changed', next);
    pushShellChrome();
    profile.mode = next;
    saveProfile(profile);
  };

  shellApi._togglePluginPane = (pluginId, localId) => {
    const entry = shellApi._plugins.get(pluginId);
    if (!entry) return;
    const handle = entry.ctx.getPane(localId);
    if (!handle) return;
    // Enter overlay first so overlay-only panes can actually show/hide.
    if (shellApi.mode === 'game') {
      shellApi.setMode('overlay');
    }
    handle.toggleVisibility();
  };

  // Unified in-process toast bus. Surfaces/plugins call notify(); the
  // notifications plugin renders. No system D-Bus — atrium-local only.
  let _notifySeq = 1;
  shellApi.notify = (opts = {}) => {
    const id = opts.id || _notifySeq++;
    const payload = {
      id,
      title: String(opts.title || opts.summary || ''),
      body: String(opts.body || ''),
      icon: opts.icon || null,
      urgency: opts.urgency || 'normal', // low | normal | critical
      timeoutMs:
        opts.timeoutMs != null
          ? Number(opts.timeoutMs)
          : opts.urgency === 'critical'
            ? 8000
            : 4500,
      source: opts.source || null,
      actions: Array.isArray(opts.actions) ? opts.actions : [],
      at: Date.now(),
    };
    shellApi.emit('notify', payload);
    return id;
  };

  function pushShellChrome() {
    const payload = {
      mode: shellApi.mode,
      fullscreen: !!cfg.fullscreen,
      game: gameLabel,
      buttons: shellApi._buttons.map((b) => ({
        id: b.id,
        label: b.label,
        shortcut: b.shortcut || null,
        pluginId: b.pluginId || null,
        action: b.action || null,
      })),
    };
    try {
      win.pane.executeScript(`
        window.dispatchEvent(new CustomEvent('atrium-shell-chrome', {
          detail: ${JSON.stringify(payload)}
        }));
      `);
    } catch (_) {}
  }

  function toggleFullscreen() {
    cfg._fullscreenOn = !cfg._fullscreenOn;
    try {
      win.setFullscreen(!!cfg._fullscreenOn);
    } catch (e) {
      console.warn('[atrium] fullscreen toggle failed:', e.message);
    }
    pushShellChrome();
  }

  function handleHostHotkey(id) {
    if (id === 'toggle-mode') {
      shellApi.setMode(shellApi.mode === 'game' ? 'overlay' : 'game');
      return;
    }
    if (id === 'toggle-fullscreen') {
      toggleFullscreen();
      return;
    }
    if (id === 'discord-sidecar:toggle') {
      shellApi._togglePluginPane('discord-sidecar', 'app');
      return;
    }
    if (id === 'perf-hud:toggle') {
      shellApi._togglePluginPane('perf-hud', 'main');
      return;
    }
    // Generic plugin action: "<pluginId>:toggle" → toggle-pane:app/main if present
    if (typeof id === 'string' && id.endsWith(':toggle')) {
      const pluginId = id.slice(0, -':toggle'.length);
      const entry = shellApi._plugins.get(pluginId);
      if (entry) {
        const local =
          entry.ctx.getPane('app') ? 'app' : entry.ctx.getPane('main') ? 'main' : null;
        if (local) shellApi._togglePluginPane(pluginId, local);
      }
    }
  }

  function installHostHotkeys() {
    const binds = resolveKeybinds(profile);
    try {
      win.clearHostHotkeys();
    } catch (_) {}
    for (const [id, spec] of Object.entries(binds)) {
      if (!spec || !spec.key) continue;
      try {
        win.registerHostHotkey({
          id,
          key: spec.key,
          modifiers: spec.modifiers || {},
        });
      } catch (e) {
        console.warn(`[atrium] registerHostHotkey ${id} failed:`, e.message);
      }
    }
  }

  // Built-in shell buttons (same handlers as host hotkeys)
  shellApi.registerButton({
    id: 'mode',
    label: 'Game · F12',
    shortcut: 'F12',
    pluginId: 'shell',
    onClick: () => handleHostHotkey('toggle-mode'),
  });
  shellApi.registerButton({
    id: 'fullscreen',
    label: 'Full · F11',
    shortcut: 'F11',
    pluginId: 'shell',
    onClick: () => handleHostHotkey('toggle-fullscreen'),
  });

  // Deduplicate Discord pills — manifest registers one; ignore extras without action.
  const _origRegister = shellApi.registerButton.bind(shellApi);
  shellApi.registerButton = (btn) => {
    if (
      btn.pluginId === 'discord-sidecar' &&
      shellApi._buttons.some((b) => b.pluginId === 'discord-sidecar')
    ) {
      return;
    }
    _origRegister(btn);
  };

  win.once('ready-to-show', async () => {
    win.show();
    pinShellBar();
    if (!cfg.sizeExplicit) {
      const w = win._logicalWidth || cfg.width;
      const h = win._logicalHeight || cfg.height;
      if (w > 0 && h > 0 && (w !== cfg.gameWidth || h !== cfg.gameHeight)) {
        cfg.gameWidth = w;
        cfg.gameHeight = h;
        process.env.ATRIUM_GAME_WIDTH = String(w);
        process.env.ATRIUM_GAME_HEIGHT = String(h);
        console.log(`[lotus-atrium] absorbed host size ${w}x${h}`);
        pinShellBar();
      }
    }
    if (cfg.fullscreen) {
      try {
        win.setFullscreen(true);
        cfg._fullscreenOn = true;
      } catch (e) {
        console.warn('[lotus-atrium] fullscreen failed:', e.message);
      }
    }

    // Host hotkeys may register before nest is ready (EC3).
    installHostHotkeys();

    // Shell owns the game pane — must be first external pane (primary).
    // createExternalAppPane + setGameMode share the winit FIFO (EC4).
    win.createExternalAppPane('game_surface', {
      command: cfg.command,
      args: cfg.args,
      anchor: Anchor.Fill,
      zIndex: 0,
      visible: true,
    });
    // Re-pin after game docks in — layout churn can stretch main.
    pinShellBar();

    win.setGameMode(shellApi.mode === 'game');

    const disabled = new Set();
    if (!cfg.discord) disabled.add('discord-sidecar');

    await loadPlugins({
      pluginsDir: layout.pluginsDir,
      bundledDir: bundledPluginsDir,
      win,
      shell: shellApi,
      profile,
      Anchor,
      disabled,
    });
    pinShellBar();

    // Fan-out nest:hotkey + telemetry (+ surface-size refits).
    wireWindowIpc(win, shellApi, {
      refitOverlays,
      cfg,
      onHostHotkey: handleHostHotkey,
    });

    // Smoke the toast bus once plugins are up (surfaces can also AtriumPlugin.notify).
    setTimeout(() => {
      try {
        shellApi.notify({
          title: 'Atrium ready',
          body: 'Unified toasts are live — plugins call ctx.notify().',
          source: 'shell',
          urgency: 'normal',
          timeoutMs: 3500,
        });
      } catch (_) {}
    }, 800);

    setTimeout(() => {
      pinShellBar();
      pushShellChrome();
    }, 300);
  });

  const onHostResize = (payload) => {
    if (payload && payload.logicalWidth && payload.logicalHeight) {
      win._logicalWidth = payload.logicalWidth;
      win._logicalHeight = payload.logicalHeight;
    } else if (payload && payload.width && payload.height) {
      const scale = win._scaleFactor || 1;
      win._logicalWidth = Math.round(payload.width / scale);
      win._logicalHeight = Math.round(payload.height / scale);
    }
    refitOverlays('host-resize');
  };
  win.on('resized', onHostResize);
  win.on('resize', onHostResize);

  win.on('closed', () => {
    setTimeout(() => process.exit(0), 400);
  });

  // Lotus ipcMain emits (payload, clientId) — NOT Electron's (event, payload).
  ipcMain.on('set-game-mode', (gameMode) => {
    shellApi.setMode(gameMode ? 'game' : 'overlay');
  });
  ipcMain.on('set-fullscreen', (enabled) => {
    win.setFullscreen(!!enabled);
    cfg._fullscreenOn = !!enabled;
    pushShellChrome();
  });
  ipcMain.on('atrium-shell-button', (buttonId) => {
    const id = typeof buttonId === 'string' ? buttonId : buttonId?.id;
    const btn = shellApi._buttons.find((b) => b.id === id);
    if (btn?.onClick) btn.onClick();
    else if (btn?.action?.startsWith('toggle-pane:') && btn.pluginId) {
      shellApi._togglePluginPane(btn.pluginId, btn.action.slice('toggle-pane:'.length));
    }
  });
  ipcMain.on('atrium-plugin-ipc', (msg) => {
    if (!msg || !msg.plugin) return;
    // Surfaces can push toasts without round-tripping their own host handler.
    if (msg.channel === 'notify' || msg.channel === '__notify') {
      shellApi.notify({ ...(msg.data || {}), source: msg.plugin });
      return;
    }
    const entry = shellApi._plugins.get(msg.plugin);
    if (entry) entry.ctx._handleHostIpc(msg.channel, msg.data);
  });
  ipcMain.on('atrium-notify', (payload) => {
    const p = payload || {};
    // Prefer stable dbus id when the host bridge paired Notify↔return.
    if (p.dbusId != null && p.id == null) {
      p.id = `dbus:${p.dbusId}`;
    }
    shellApi.notify(p);
  });
  ipcMain.on('atrium-notify-closed', (payload) => {
    shellApi.emit('notify-closed', payload || {});
  });
  ipcMain.on('atrium-notify-action', (payload) => {
    shellApi.emit('notify-action', payload || {});
  });
  return { win, shell: shellApi, profile };
}

function wireWindowIpc(win, shellApi, opts = {}) {
  const refitOverlays = opts.refitOverlays || (() => {});
  const cfg = opts.cfg || {};
  const onHostHotkey = opts.onHostHotkey || (() => {});
  // Renderer events arrive on the window; shell.html forwards nest:hotkey / stats to Node.
  ipcMain.on('nest-hotkey', (payload) => {
    const id = payload && payload.id;
    if (id) onHostHotkey(id);
  });
  ipcMain.on('atrium-forward-stats', (stats) => {
    if (!stats || typeof stats !== 'object') return;
    for (const cb of shellApi._telemetrySubs) {
      try {
        cb(stats);
      } catch (_) {}
    }

    // Game buffer size is the resolution atrium is effectively presenting.
    // When it changes (1600×900 ↔ 1920×1080), sync host layout + overlay panes.
    const sw = Number(stats.width) | 0;
    const sh = Number(stats.height) | 0;
    if (sw < 640 || sh < 360) return;
    const prev = shellApi._surfaceSize;
    if (prev && prev.w === sw && prev.h === sh) return;
    shellApi._surfaceSize = { w: sw, h: sh };
    win._gameSurface = { width: sw, height: sh };

    const lw = Number(win._logicalWidth) | 0;
    const lh = Number(win._logicalHeight) | 0;
    const drift = Math.abs(lw - sw) > 8 || Math.abs(lh - sh) > 8;
    if (drift) {
      try {
        win.setSize(sw, sh);
        win._logicalWidth = sw;
        win._logicalHeight = sh;
        cfg.gameWidth = sw;
        cfg.gameHeight = sh;
        process.env.ATRIUM_GAME_WIDTH = String(sw);
        process.env.ATRIUM_GAME_HEIGHT = String(sh);
      } catch (e) {
        console.warn('[atrium] host sync to surface size failed:', e.message);
      }
    }
    refitOverlays(drift ? `surface ${sw}x${sh}` : `surface-stable ${sw}x${sh}`);
  });
  ipcMain.on('atrium-forward-mode', (data) => {
    if (data && data.game_mode !== undefined) {
      const mode = data.game_mode ? 'game' : 'overlay';
      if (mode !== shellApi.mode) {
        shellApi.mode = mode;
        for (const { ctx } of shellApi._plugins.values()) {
          ctx._applyShellMode(mode);
        }
        shellApi.emit('mode-changed', mode);
        pushChromeSafe(win, shellApi);
        refitOverlays('mode-change');
      }
    }
  });
}

function pushChromeSafe(win, shellApi) {
  try {
    const payload = {
      mode: shellApi.mode,
      buttons: shellApi._buttons.map((b) => ({
        id: b.id,
        label: b.label,
        shortcut: b.shortcut || null,
        pluginId: b.pluginId || null,
        action: b.action || null,
      })),
    };
    win.pane.executeScript(`
      window.dispatchEvent(new CustomEvent('atrium-shell-chrome', {
        detail: ${JSON.stringify(payload)}
      }));
    `);
  } catch (_) {}
}

module.exports = { startShell, TOP_CHROME };
