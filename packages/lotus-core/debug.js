const { ServoWindow, app } = require('@lotus-gui/core');
app.warmup();
const win = new ServoWindow({
    id: 'test-window',
    width: 800,
    height: 600,
    title: "Test Window"
});
console.log("Handle methods:", Object.getOwnPropertyNames(Object.getPrototypeOf(win.handle)));
process.exit(0);
