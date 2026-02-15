const { ServoWindow, app } = require('./lotus.js');

console.log("Initializing Lotus Runtime...");

// Initialize Backend
app.warmup();

const win = new ServoWindow({
    width: 800,
    height: 600,
    title: "Lotus Example"
});

win.on('ready', () => {
    console.log("Window is ready!");

    // Demo: Load URL after 1 second
    setTimeout(() => {
        console.log("Navigating to Google...");
        win.loadUrl("https://google.com");
    }, 1000);
});

// Keep Node alive
setInterval(() => { }, 1000);
