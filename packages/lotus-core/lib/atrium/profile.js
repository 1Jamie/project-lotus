'use strict';

const fs = require('fs');
const path = require('path');
const os = require('os');

const TOP_CHROME = 56;

function configDir() {
  const xdg = process.env.XDG_CONFIG_HOME;
  const base = xdg ? xdg : path.join(os.homedir(), '.config');
  return path.join(base, 'lotus-atrium');
}

function profilePath() {
  return path.join(configDir(), 'overlay-profile.json');
}

function pluginsDir() {
  return path.join(configDir(), 'plugins');
}

/** Default host hotkeys (W3C KeyboardEvent.code). Overridable via profile.keybinds. */
function defaultKeybinds() {
  return {
    'toggle-mode': { key: 'F12' },
    'toggle-fullscreen': { key: 'F11' },
    'discord-sidecar:toggle': { key: 'F9' },
    'perf-hud:toggle': { key: 'F10' },
  };
}

function defaultProfile() {
  return {
    version: 2,
    mode: 'game',
    plugins: {},
    keybinds: defaultKeybinds(),
    panes: {},
  };
}

/** Merge saved keybinds over defaults (missing ids keep defaults). */
function resolveKeybinds(profile) {
  const defaults = defaultKeybinds();
  const saved = profile && typeof profile.keybinds === 'object' ? profile.keybinds : {};
  const out = { ...defaults };
  for (const [id, spec] of Object.entries(saved)) {
    if (!spec || typeof spec !== 'object') continue;
    if (typeof spec.key !== 'string' || !spec.key) continue;
    out[id] = {
      key: spec.key,
      modifiers: spec.modifiers && typeof spec.modifiers === 'object' ? spec.modifiers : {},
    };
  }
  return out;
}

function ensureConfigLayout(bundledPluginsDir) {
  const dir = configDir();
  const plugs = pluginsDir();
  fs.mkdirSync(plugs, { recursive: true });
  const profile = profilePath();
  if (!fs.existsSync(profile)) {
    fs.writeFileSync(profile, JSON.stringify(defaultProfile(), null, 2));
  }
  // Copy missing first-party plugins from the in-repo tree.
  if (bundledPluginsDir && fs.existsSync(bundledPluginsDir)) {
    for (const name of fs.readdirSync(bundledPluginsDir)) {
      const src = path.join(bundledPluginsDir, name);
      const dest = path.join(plugs, name);
      if (!fs.statSync(src).isDirectory()) continue;
      if (fs.existsSync(dest)) continue;
      copyDir(src, dest);
    }
  }
  return { configDir: dir, pluginsDir: plugs, profilePath: profile };
}

function copyDir(src, dest) {
  fs.mkdirSync(dest, { recursive: true });
  for (const entry of fs.readdirSync(src, { withFileTypes: true })) {
    const s = path.join(src, entry.name);
    const d = path.join(dest, entry.name);
    if (entry.isDirectory()) copyDir(s, d);
    else fs.copyFileSync(s, d);
  }
}

function loadProfile() {
  try {
    const raw = fs.readFileSync(profilePath(), 'utf8');
    return { ...defaultProfile(), ...JSON.parse(raw) };
  } catch {
    return defaultProfile();
  }
}

function saveProfile(profile) {
  fs.mkdirSync(configDir(), { recursive: true });
  fs.writeFileSync(profilePath(), JSON.stringify(profile, null, 2));
}

function pluginStorage(profile, pluginId) {
  if (!profile.plugins[pluginId]) {
    profile.plugins[pluginId] = { enabled: true, storage: {} };
  }
  if (!profile.plugins[pluginId].storage) {
    profile.plugins[pluginId].storage = {};
  }
  return {
    get(key, fallback) {
      const v = profile.plugins[pluginId].storage[key];
      return v === undefined ? fallback : v;
    },
    set(key, value) {
      profile.plugins[pluginId].storage[key] = value;
      saveProfile(profile);
    },
  };
}

function paneKey(pluginId, localId) {
  return `${pluginId}:${localId}`;
}

function getPaneRect(profile, pluginId, localId) {
  return profile.panes[paneKey(pluginId, localId)] || null;
}

function setPaneRect(profile, pluginId, localId, rect) {
  profile.panes[paneKey(pluginId, localId)] = {
    x: rect.x,
    y: rect.y,
    width: rect.width,
    height: rect.height,
    visible: rect.visible,
  };
  saveProfile(profile);
}

module.exports = {
  TOP_CHROME,
  configDir,
  profilePath,
  pluginsDir,
  ensureConfigLayout,
  loadProfile,
  saveProfile,
  pluginStorage,
  getPaneRect,
  setPaneRect,
  paneKey,
  defaultKeybinds,
  resolveKeybinds,
};
