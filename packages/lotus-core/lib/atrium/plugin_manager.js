'use strict';

const fs = require('fs');
const path = require('path');
const { createPluginContext } = require('./plugin_context');
const { resetLayerCounters } = require('./layers');

/**
 * Discover + activate atrium plugins from a plugins directory.
 */
async function loadPlugins({
  pluginsDir,
  bundledDir,
  win,
  shell,
  profile,
  Anchor,
  disabled = new Set(),
}) {
  resetLayerCounters();
  const loaded = [];

  const dirs = [];
  // Prefer in-repo bundled plugins during development when present.
  const scanRoots = [];
  if (bundledDir && fs.existsSync(bundledDir)) scanRoots.push(bundledDir);
  if (pluginsDir && fs.existsSync(pluginsDir) && pluginsDir !== bundledDir) {
    scanRoots.push(pluginsDir);
  }

  const seen = new Set();
  for (const root of scanRoots) {
    for (const name of fs.readdirSync(root)) {
      if (seen.has(name)) continue;
      const dir = path.join(root, name);
      if (!fs.statSync(dir).isDirectory()) continue;
      const manifestPath = path.join(dir, 'plugin.json');
      if (!fs.existsSync(manifestPath)) continue;
      seen.add(name);
      dirs.push({ name, dir, manifestPath });
    }
  }

  for (const { name, dir, manifestPath } of dirs) {
    let manifest;
    try {
      manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
    } catch (e) {
      console.warn(`[atrium] skip plugin ${name}: bad plugin.json (${e.message})`);
      continue;
    }
    const pluginId = manifest.id || name;
    if (disabled.has(pluginId) || profile.plugins?.[pluginId]?.enabled === false) {
      console.log(`[atrium] plugin disabled: ${pluginId}`);
      continue;
    }

    const ctx = createPluginContext({
      pluginId,
      pluginDir: dir,
      manifest,
      win,
      shell,
      profile,
      Anchor,
    });

    // Register declarative shell buttons before activate (host may add more).
    for (const btn of manifest.shellButtons || []) {
      shell.registerButton({
        id: `${pluginId}:${btn.id || 'btn'}`,
        label: btn.label,
        shortcut: btn.shortcut,
        action: btn.action,
        pluginId,
        onClick: () => {
          if (typeof btn.action === 'string' && btn.action.startsWith('toggle-pane:')) {
            shell._togglePluginPane(pluginId, btn.action.slice('toggle-pane:'.length));
          }
        },
      });
    }

    try {
      if (manifest.host) {
        const hostPath = path.join(dir, manifest.host);
        // eslint-disable-next-line import/no-dynamic-require, global-require
        const mod = require(hostPath);
        if (typeof mod.activate === 'function') {
          await mod.activate(ctx);
        }
        // Host may create panes; also mount any remaining declarative surfaces.
        await ctx._autoMountManifestPanes();
      } else {
        await ctx._autoMountManifestPanes();
      }
    } catch (e) {
      console.error(`[atrium] plugin ${pluginId} failed to activate:`, e);
      continue;
    }

    loaded.push({ pluginId, ctx, manifest, dir });
    shell._plugins.set(pluginId, { ctx, manifest });
    console.log(`[atrium] activated plugin ${pluginId}`);
  }

  return loaded;
}

module.exports = { loadPlugins };
