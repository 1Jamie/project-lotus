const { ServoWindow, app, LayoutBuilder, ipcMain } = require('@lotus-gui/core');
const path = require('path');

console.log("[Showcase] Warming up backend...");
app.warmup();

const UI_DIR = path.join(__dirname, 'ui');
const INITIAL_WIDTH = 1200;
const INITIAL_HEIGHT = 800;

// 1. Declarative Layout & Overlays
const layout = new LayoutBuilder()
    // 0. Window Controls (Frameless)
    .top('controls', 40, { url: `lotus-resource://localhost/controls.html` })
    .left('sidebar', 280, { url: `lotus-resource://localhost/sidebar.html` })
    .bottom('terminal', 250, { url: `lotus-resource://localhost/terminal.html`, visible: false })
    .fill('content', { url: `lotus-resource://localhost/content.html` })
    // Absolute overlay (Command Palette) centered manually later
    .absolute('palette', 0, 0, 600, 400, { 
        url: `lotus-resource://localhost/palette.html`, 
        zIndex: 100, 
        visible: false 
    });

const win = new ServoWindow({
    id: 'multi-pane-window',
    width: INITIAL_WIDTH,
    height: INITIAL_HEIGHT,
    title: "Lotus Multi-Pane Showcase",
    root: UI_DIR,
    visible: false,
    frameless: true,
    transparent: true,
    cornerRadius: 20,
    autoResizeMain: false,
    ...layout.config()
});

let terminalVisible = false;
let paletteVisible = false;

win.once('ready-to-show', () => {
    console.log("[Showcase] Window READY - Displaying natively synchronized UI...");
    win.show();
    centerPalette(INITIAL_WIDTH, INITIAL_HEIGHT);
});

function centerPalette(width, height) {
    const palette = win.panes.get('palette');
    if (palette) {
        const pw = 600;
        const ph = 400;
        palette.setRect((width - pw) / 2, (height - ph) / 2, pw, ph);
    }
}

// Ensure overlay stays centered when window resizes
win.on('resize', (payload) => {
    centerPalette(payload.logicalWidth, payload.logicalHeight);
});

// 3. IPC Orchestration
ipcMain.on('sidebar-navigate', (data) => {
    console.log(`[Showcase] Navigation requested: ${data ? data.view : 'unknown'}`);
    // Route message to content pane - ensure data is at least an empty object
    win.sendToPaneRenderer('content', 'update-view', data || {});
});

ipcMain.on('toggle-terminal', () => {
    const term = win.panes.get('terminal');
    if (term) {
        terminalVisible = !terminalVisible;
        console.log(`[Showcase] Toggling terminal visibility to: ${terminalVisible}`);
        term.setVisible(terminalVisible);
        // The native Rust loop will automatically recalculate the layout and expand 'content'
        // to fill the void, or shrink it to make room for the terminal.
    }
});

ipcMain.on('toggle-palette', () => {
    const pal = win.panes.get('palette');
    if (pal) {
        paletteVisible = !paletteVisible;
        console.log(`[Showcase] Toggling palette visibility to: ${paletteVisible}`);
        pal.setVisible(paletteVisible);
        if (paletteVisible) pal.focus();
    }
});

ipcMain.on('palette-close', () => {
    paletteVisible = false;
    win.panes.get('palette').setVisible(false);
    win.panes.get('content').focus();
});

// 4. Window Controls IPC
ipcMain.on('window-close', () => {
    win.emit('close');
});

ipcMain.on('window-minimize', () => {
    win.minimize();
});

ipcMain.on('window-maximize', () => {
    win.maximize();
});

win.on('close', () => {
    console.log("[Showcase] Closing sequence initiated.");
    win.close();
    setTimeout(() => process.exit(0), 100);
});

console.log('[Showcase] Initialized. Waiting for native layout sync...');
