const { execSync } = require('child_process');
const path = require('path');
const os = require('os');
const fs = require('fs');

const scriptDir = __dirname;

console.log('[Lotus] Checking for Servo engine patches...');

try {
  if (os.platform() === 'win32') {
    const psScript = path.join(scriptDir, 'apply-patches.ps1');
    if (fs.existsSync(psScript)) {
      console.log(`[Lotus] Running Windows PowerShell patch script...`);
      execSync(`powershell -ExecutionPolicy Bypass -File "${psScript}"`, { stdio: 'inherit' });
    } else {
      console.error(`[Lotus] PowerShell patch script not found at ${psScript}`);
    }
  } else {
    const shScript = path.join(scriptDir, 'apply-patches.sh');
    if (fs.existsSync(shScript)) {
      console.log(`[Lotus] Running Unix Shell patch script...`);
      try { fs.chmodSync(shScript, '755'); } catch (e) {}
      execSync(`bash "${shScript}"`, { stdio: 'inherit' });
    } else {
      console.error(`[Lotus] Shell patch script not found at ${shScript}`);
    }
  }
} catch (e) {
  console.warn('[Lotus] Patch application process encountered an issue. It might already be applied.');
}
