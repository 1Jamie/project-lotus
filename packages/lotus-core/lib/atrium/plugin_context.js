'use strict';

const path = require('path');
const { pathToFileURL } = require('url');
const { zIndexForLayer } = require('./layers');
const { getPaneRect, setPaneRect, pluginStorage } = require('./profile');
const { clampRect, windowBounds } = require('./bounds');

/**
 * Build a namespaced PluginContext for one plugin.
 */
function createPluginContext({
  pluginId,
  pluginDir,
  manifest,
  win,
  shell,
  profile,
  Anchor,
}) {
  const panes = new Map(); // localId → { pane, meta }
  const ipcHandlers = new Map();
  const lastIpc = new Map(); // channel → last payload (flushed on reshown)
  const storage = pluginStorage(profile, pluginId);

  function prefixedId(localId) {
    // Discord F9 bridge: allow rawPaneId override (legacy "discord").
    return localId;
  }

  function resolvePaneId(localId, opts = {}) {
    if (opts.rawPaneId) return opts.rawPaneId;
    return `plugin:${pluginId}:${localId}`;
  }

  function deliverIpc(pane, channel, data) {
    if (!pane || typeof pane.executeScript !== 'function') return;
    const full = `plugin:${pluginId}:${channel}`;
    pane.executeScript(`
      window.dispatchEvent(new CustomEvent(${JSON.stringify(full)}, {
        detail: ${JSON.stringify(data)}
      }));
      if (window.AtriumPlugin && typeof window.AtriumPlugin._emit === 'function') {
        window.AtriumPlugin._emit(${JSON.stringify(channel)}, ${JSON.stringify(data)});
      }
    `);
  }

  /** Push cached IPC + wake the surface after hide→show (Servo drops updates while hidden). */
  function flushPaneAfterShow(pane) {
    if (!pane || !pane.visible) return;
    for (const [channel, data] of lastIpc) {
      try {
        deliverIpc(pane, channel, data);
      } catch (e) {
        ctx.log('ipc flush failed:', e.message);
      }
    }
    try {
      pane.executeScript(`window.dispatchEvent(new Event('atrium-pane-shown'));`);
    } catch (_) {}
  }

  function applyModeFlags(pane, meta, mode) {
    if (!pane) return;
    const interactive = meta.interactive || 'overlay-only';
    if (interactive === 'never') {
      pane.setPassthrough(true);
    } else if (interactive === 'overlay-only') {
      pane.setPassthrough(mode === 'game');
    } else {
      pane.setPassthrough(false);
    }
    if (meta.visibility === 'overlay-only') {
      const want = mode === 'game' ? false : !!meta.userVisible;
      // Skip no-op SetPaneVisible — re-entering after Rust F9 caused open↔close flicker.
      const wasVisible = !!pane.visible;
      if (pane.visible !== want) {
        pane.setVisible(want);
      }
      if (want && !wasVisible) {
        flushPaneAfterShow(pane);
      }
    }
  }

  const ctx = {
    id: pluginId,
    dir: pluginDir,
    manifest,
    log(...args) {
      console.log(`[atrium:${pluginId}]`, ...args);
    },
    storage,
    shell,
    ipc: {
      send(channel, data) {
        lastIpc.set(channel, data);
        for (const { pane } of panes.values()) {
          // Never evaluate into a hidden Servo pane — it wedges telemetry until restart.
          if (!pane || !pane.visible) continue;
          try {
            deliverIpc(pane, channel, data);
          } catch (e) {
            ctx.log('ipc send failed:', e.message);
          }
        }
      },
      on(channel, handler) {
        ipcHandlers.set(channel, handler);
      },
    },
    telemetry: {
      onFrameStats(cb) {
        shell._telemetrySubs.push(cb);
      },
    },

    /** Push a toast into the unified atrium notification surface. */
    notify(opts = {}) {
      if (typeof shell.notify !== 'function') {
        ctx.log('notify: shell.notify unavailable');
        return null;
      }
      return shell.notify({ ...opts, source: opts.source || pluginId });
    },

    getPane(localId) {
      const entry = panes.get(localId);
      return entry ? wrapPaneHandle(localId, entry.pane, entry.meta) : null;
    },

    async createSurfacePane(localId, options = {}) {
      const paneId = resolvePaneId(localId, options);
      const layer = options.layer || 'hud';
      const zIndex = zIndexForLayer(layer, options.zIndex);
      const entry = options.entry || (manifest.panes?.[localId]?.entry);
      if (!entry) throw new Error(`createSurfacePane('${localId}'): missing entry`);
      const fileUrl = pathToFileURL(path.join(pluginDir, entry)).href;
      // Always size from the live host window — each atrium session (game +
      // resolution) is its own layout; never restore geometry from a prior run.
      const saved = getPaneRect(profile, pluginId, localId);
      const def = options.defaultRect || manifest.panes?.[localId]?.defaultRect || {};
      const rect = clampRect(
        {
          x: def.x ?? 20,
          y: def.y ?? 20,
          width: def.width ?? 280,
          height: def.height ?? 200,
        },
        windowBounds(win)
      );
      const visibility = options.visibility || manifest.panes?.[localId]?.visibility || 'always';
      const interactive =
        options.interactive || manifest.panes?.[localId]?.interactive || 'overlay-only';
      const startVisible =
        visibility === 'overlay-only' ? shell.mode !== 'game' : saved?.visible !== false;

      const pane = win.createPane(paneId, {
        url: fileUrl,
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
        zIndex,
        anchor: Anchor.None,
        visible: startVisible,
        pointerPassthrough: interactive === 'never',
      });

      // Inject tiny AtriumPlugin bridge into the surface.
      setTimeout(() => {
        try {
          pane.executeScript(`
            (function(){
              if (window.AtriumPlugin) return;
              const _h = {};
              window.AtriumPlugin = {
                id: ${JSON.stringify(pluginId)},
                pane: ${JSON.stringify(localId)},
                on(ch, fn) { (_h[ch] = _h[ch] || []).push(fn); },
                _emit(ch, data) { (_h[ch] || []).forEach(fn => { try { fn(data); } catch(e){} }); },
                send(ch, data) {
                  // Host-bound channels go through lotus IPC if present
                  const ipc = window.lotus?.ipc || window.lotus;
                  if (ipc && ipc.send) ipc.send('atrium-plugin-ipc', {
                    plugin: ${JSON.stringify(pluginId)},
                    channel: ch,
                    data
                  });
                },
                notify(opts) {
                  const ipc = window.lotus?.ipc || window.lotus;
                  if (ipc && ipc.send) {
                    ipc.send('atrium-plugin-ipc', {
                      plugin: ${JSON.stringify(pluginId)},
                      channel: 'notify',
                      data: opts || {}
                    });
                  }
                }
              };
            })();
          `);
        } catch (_) {}
      }, 200);

      const meta = {
        localId,
        paneId,
        layer,
        visibility,
        interactive,
        userVisible: startVisible,
        reposition: options.reposition,
      };
      panes.set(localId, { pane, meta });
      applyModeFlags(pane, meta, shell.mode);
      return wrapPaneHandle(localId, pane, meta);
    },

    async createExternalPane(localId, options = {}) {
      const paneId = resolvePaneId(localId, options);
      const layer = options.layer || 'drawer';
      const zIndex = zIndexForLayer(layer, options.zIndex);
      // Geometry is always recomputed for this host size (Discord drawer, etc.).
      // Profile only remembers userVisible — not x/y/w/h from another game/res.
      const saved = getPaneRect(profile, pluginId, localId);
      const def = options.defaultRect || {};
      const { x, y, width, height } = clampRect(
        {
          x: def.x ?? 0,
          y: def.y ?? 56,
          width: def.width ?? 400,
          height: def.height ?? 600,
        },
        windowBounds(win)
      );
      const visibility = options.visibility || 'overlay-only';
      const interactive = options.interactive || 'overlay-only';
      const startVisible =
        visibility === 'overlay-only' ? false : saved?.visible !== false;

      const dbus = options.dbus || {};
      const pane = win.createExternalAppPane(paneId, {
        command: options.command,
        args: options.args || [],
        x,
        y,
        width,
        height,
        zIndex,
        anchor: Anchor.None,
        visible: startVisible,
        pointerPassthrough: interactive === 'never',
        dbus: {
          notifications: dbus.notifications || 'off',
          notificationApps: Array.isArray(dbus.notificationApps)
            ? dbus.notificationApps
            : [],
        },
      });

      const meta = {
        localId,
        paneId,
        layer,
        visibility,
        interactive,
        userVisible: saved?.visible === true,
        reposition: options.reposition,
      };
      panes.set(localId, { pane, meta });
      applyModeFlags(pane, meta, shell.mode);
      return wrapPaneHandle(localId, pane, meta);
    },

    _applyShellMode(mode) {
      for (const { pane, meta } of panes.values()) {
        applyModeFlags(pane, meta, mode);
      }
    },

    _onWindowResized() {
      for (const { pane, meta } of panes.values()) {
        try {
          if (typeof meta.reposition === 'function') {
            meta.reposition(pane, win);
          } else {
            // Absolute panes without a reposition hook still get clamped.
            const b = windowBounds(win);
            const cur = {
              x: pane._layout?.x ?? 0,
              y: pane._layout?.y ?? 0,
              width: pane._layout?.width ?? b.width,
              height: pane._layout?.height ?? b.height,
            };
            const next = clampRect(cur, b);
            if (
              next.x !== cur.x ||
              next.y !== cur.y ||
              next.width !== cur.width ||
              next.height !== cur.height
            ) {
              pane.setRect(next.x, next.y, next.width, next.height);
            }
          }
        } catch (e) {
          ctx.log('reposition failed:', e.message);
        }
      }
    },

    _handleHostIpc(channel, data) {
      const fn = ipcHandlers.get(channel);
      if (fn) fn(data);
    },
  };

  function wrapPaneHandle(localId, pane, meta) {
    return {
      id: meta.paneId,
      localId,
      get visible() {
        return pane.visible;
      },
      /**
       * Mirror visibility already applied by the host (Rust F9 bridge).
       * Must NOT call pane.setVisible — that re-enters SetPaneVisible and
       * open↔closes Discord on a single keypress.
       */
      syncHostVisible(v) {
        const want = !!v;
        meta.userVisible = want;
        pane.visible = want;
        // F9 / Rust visibility sync — refit drawer to current host size so a
        // profile rect from a larger resolution cannot sit past the right edge.
        if (want && typeof meta.reposition === 'function') {
          try {
            meta.reposition(
              {
                setRect: (x, y, width, height) => {
                  pane.setRect(x, y, width, height);
                  setPaneRect(profile, pluginId, localId, {
                    x,
                    y,
                    width,
                    height,
                    visible: want,
                  });
                },
              },
              win
            );
            return;
          } catch (e) {
            ctx.log('syncHostVisible reposition failed:', e.message);
          }
        }
        setPaneRect(profile, pluginId, localId, {
          x: pane._layout?.x ?? 0,
          y: pane._layout?.y ?? 0,
          width: pane._layout?.width ?? 0,
          height: pane._layout?.height ?? 0,
          visible: want,
        });
      },
      setVisible(v) {
        const wasVisible = !!pane.visible;
        meta.userVisible = !!v;
        // If the user wants it shown while still in game mode, flip to overlay
        // first — otherwise overlay-only panes would be force-hidden and look
        // like "can't open/close Discord".
        if (meta.visibility === 'overlay-only' && meta.userVisible && shell.mode === 'game') {
          shell.setMode('overlay');
        }
        // Refit before show so a 1920-era saved drawer cannot open off-screen
        // on a 1600×900 (or gamescope-absorbed) host.
        if (meta.userVisible && typeof meta.reposition === 'function') {
          try {
            meta.reposition(
              {
                setRect: (x, y, width, height) => {
                  pane.setRect(x, y, width, height);
                  setPaneRect(profile, pluginId, localId, {
                    x,
                    y,
                    width,
                    height,
                    visible: meta.userVisible,
                  });
                },
              },
              win
            );
          } catch (e) {
            ctx.log('pre-show reposition failed:', e.message);
          }
        }
        if (meta.visibility === 'overlay-only' && shell.mode === 'game') {
          if (pane.visible) pane.setVisible(false);
        } else if (pane.visible !== !!meta.userVisible) {
          pane.setVisible(!!meta.userVisible);
        }
        setPaneRect(profile, pluginId, localId, {
          x: pane._layout?.x ?? 0,
          y: pane._layout?.y ?? 0,
          width: pane._layout?.width ?? 0,
          height: pane._layout?.height ?? 0,
          visible: meta.userVisible,
        });
        if (pane.visible && !wasVisible) {
          flushPaneAfterShow(pane);
        }
      },
      toggleVisibility() {
        const next = !(pane.visible || meta.userVisible);
        this.setVisible(next);
        return next;
      },
      setRect(x, y, width, height) {
        const next = clampRect({ x, y, width, height }, windowBounds(win));
        pane.setRect(next.x, next.y, next.width, next.height);
        setPaneRect(profile, pluginId, localId, {
          x: next.x,
          y: next.y,
          width: next.width,
          height: next.height,
          visible: meta.userVisible,
        });
      },
      on() {
        /* WindowCreated deferred */
      },
      raw: pane,
    };
  }

  // Auto-create declarative surface panes from manifest (UI-only / default surfaces).
  ctx._autoMountManifestPanes = async () => {
    const decls = manifest.panes || {};
    for (const [localId, decl] of Object.entries(decls)) {
      if (decl.type === 'external') continue;
      if (panes.has(localId)) continue;
      await ctx.createSurfacePane(localId, {
        entry: decl.entry,
        defaultRect: decl.defaultRect,
        layer: decl.layer || 'hud',
        visibility: decl.visibility,
        interactive: decl.interactive,
      });
    }
  };

  return ctx;
}

module.exports = { createPluginContext };
