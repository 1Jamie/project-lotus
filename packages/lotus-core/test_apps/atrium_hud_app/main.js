#!/usr/bin/env node
/**
 * Lotus Atrium — nested Wayland compositor (gamescope-style Steam wrapper).
 *
 * Steam launch options:
 *   atrium -- %command%
 *   atrium -W 1920 -H 1080 -f -- %command%
 *
 * Keys: F12 game↔overlay, F9 Discord, F11 fullscreen
 * Plugins: ~/.config/lotus-atrium/plugins (+ in-repo defaults)
 */
const path = require('path');
const { app } = require('../../lotus.js');
const { startShell } = require('../../lib/atrium/shell');

app.warmup();
app.initVfs();

function usage(exitCode = 0) {
  console.log(`Lotus Atrium — nested compositor (Steam / gamescope-style)

  atrium [options] -- <command> [args...]

Options:
  -W, -H <n>               Nested game size (default 1920x1080)
  -w, -h <n>               Lotus window size override
  -f, --fullscreen         Start lotus window fullscreen
  -r, --refresh <Hz>       Nested refresh hint
  --discord / --no-discord Discord sidecar plugin (default: on)
  --hud                    Start in overlay input mode
  --help                   Show this help

Keys: F12 overlay · F9 Discord · F11 fullscreen
`);
  process.exit(exitCode);
}

function positiveNum(v) {
  const n = Number(v);
  return Number.isFinite(n) && n > 0 ? n : 0;
}

function isGameModeSession() {
  if (process.env.ATRIUM_GAMEMODE === '1') return true;
  if (process.env.GAMESCOPE_WAYLAND_DISPLAY) return true;
  const desk = String(process.env.XDG_CURRENT_DESKTOP || '').toLowerCase();
  return desk.includes('gamescope');
}

function absorbLauncherResolution() {
  const pairs = [
    ['ATRIUM_GAME_WIDTH', 'ATRIUM_GAME_HEIGHT'],
    ['WIDTH', 'HEIGHT'],
    ['INTERNAL_WIDTH', 'INTERNAL_HEIGHT'],
    ['SCREEN_WIDTH', 'SCREEN_HEIGHT'],
    ['GAMESCOPE_WIDTH', 'GAMESCOPE_HEIGHT'],
    ['XWAYLAND_WIDTH', 'XWAYLAND_HEIGHT'],
  ];
  for (const [wk, hk] of pairs) {
    const w = positiveNum(process.env[wk]);
    const h = positiveNum(process.env[hk]);
    if (w && h) return { w, h, source: `${wk}/${hk}` };
  }
  return null;
}

function parseArgs(argv) {
  const gameMode = isGameModeSession();
  const absorbed = absorbLauncherResolution();
  const out = {
    command: null,
    args: [],
    discord: process.env.ATRIUM_DISCORD !== '0',
    startHudMode: false,
    fullscreen:
      process.env.ATRIUM_FULLSCREEN === '1' ||
      process.env.ATRIUM_FULLSCREEN === 'true' ||
      (gameMode && process.env.ATRIUM_FULLSCREEN !== '0'),
    gameWidth: absorbed ? absorbed.w : 1920,
    gameHeight: absorbed ? absorbed.h : 1080,
    width: positiveNum(process.env.ATRIUM_WIDTH),
    height: positiveNum(process.env.ATRIUM_HEIGHT),
    refreshHz: positiveNum(process.env.ATRIUM_REFRESH),
    sizeExplicit: !!absorbed,
    sizeSource: absorbed ? absorbed.source : 'default',
    gameMode,
  };

  const raw = argv.slice(2);
  const cmd = [];
  let passthrough = false;

  const takeNum = (i, flag) => {
    const v = raw[i + 1];
    if (v == null || v.startsWith('-')) {
      console.error(`atrium: ${flag} requires a number`);
      usage(1);
    }
    const n = positiveNum(v);
    if (!n) {
      console.error(`atrium: ${flag} invalid number: ${v}`);
      usage(1);
    }
    return n;
  };

  for (let i = 0; i < raw.length; i++) {
    const a = raw[i];
    if (!passthrough) {
      if (a === '--') {
        passthrough = true;
        continue;
      }
      if (a === '--help') usage(0);
      if (a === '--discord') {
        out.discord = true;
        continue;
      }
      if (a === '--no-discord') {
        out.discord = false;
        continue;
      }
      if (a === '--hud') {
        out.startHudMode = true;
        continue;
      }
      if (a === '-f' || a === '--fullscreen') {
        out.fullscreen = true;
        continue;
      }
      if (a === '-W' || a === '--game-width') {
        out.gameWidth = takeNum(i, a);
        out.sizeExplicit = true;
        out.sizeSource = out.sizeSource === '-H' ? '-W/-H' : '-W';
        i++;
        continue;
      }
      if (a === '-H' || a === '--game-height') {
        out.gameHeight = takeNum(i, a);
        out.sizeExplicit = true;
        out.sizeSource = out.sizeSource === '-W' ? '-W/-H' : '-H';
        i++;
        continue;
      }
      if (a === '-w' || a === '--width') {
        out.width = takeNum(i, a);
        i++;
        continue;
      }
      if (a === '-h' || a === '--height') {
        // careful: -h is also help in some CLIs; atrium uses --help
        out.height = takeNum(i, a);
        i++;
        continue;
      }
      if (a === '-r' || a === '--refresh') {
        out.refreshHz = takeNum(i, a);
        i++;
        continue;
      }
      if (a.startsWith('-')) {
        console.error(`atrium: unknown option: ${a}`);
        usage(1);
      }
    }
    cmd.push(a);
  }

  if (cmd.length === 0) {
    if (process.env.ATRIUM_APP) {
      out.command = process.env.ATRIUM_APP;
      out.args = process.env.ATRIUM_ARGS
        ? process.env.ATRIUM_ARGS.split(/\s+/).filter(Boolean)
        : [];
    } else {
      out.command = 'glxgears';
      out.args = [];
    }
  } else {
    out.command = cmd[0];
    out.args = cmd.slice(1);
  }

  if (!out.width) out.width = out.gameWidth;
  if (!out.height) out.height = out.gameHeight;
  return out;
}

function shortGameLabel(command, args) {
  const base = path.basename(command || '');
  const exe = args.find((a) => /\.exe$/i.test(a));
  if (exe) return path.basename(exe).replace(/\.exe$/i, '');
  if (base === 'proton' || base === 'run' || base === 'env') {
    const next = args.find((a) => a && !a.startsWith('-') && !/=/.test(a) && !/proton/i.test(a));
    if (next) return path.basename(next).replace(/\.exe$/i, '');
  }
  if (process.env.SteamAppId) return `App ${process.env.SteamAppId}`;
  return base || 'game';
}

const cfg = parseArgs(process.argv);
cfg.gameLabel = shortGameLabel(cfg.command, cfg.args);

process.env.ATRIUM_GAME_WIDTH = String(cfg.gameWidth);
process.env.ATRIUM_GAME_HEIGHT = String(cfg.gameHeight);
if (cfg.refreshHz > 0) process.env.ATRIUM_REFRESH = String(cfg.refreshHz);

console.log(
  `[lotus-atrium] ${cfg.gameLabel}  nested ${cfg.gameWidth}x${cfg.gameHeight}` +
    `  (size=${cfg.sizeSource})` +
    (cfg.gameMode ? '  gamemode' : '') +
    (cfg.fullscreen ? '  fullscreen' : '') +
    (cfg.discord ? '  discord=F9' : '')
);

startShell(cfg).catch((err) => {
  console.error('[lotus-atrium] shell failed:', err);
  process.exit(1);
});
