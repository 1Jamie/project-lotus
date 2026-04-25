const { execSync } = require('child_process');
const fs = require('fs');
const path = require('path');

function clean() {
  console.log('Cleaning Rust artifacts...');
  try {
    execSync('cargo clean', { stdio: 'inherit' });
  } catch (err) {
    console.error('Failed to run cargo clean:', err.message);
  }

  console.log('Cleaning NAPI-RS artifacts...');
  const filesToClean = [
    'index.js',
    'index.d.ts',
  ];

  // Also clean any .node files in the root
  const rootDir = path.join(__dirname, '..');
  const files = fs.readdirSync(rootDir);
  files.forEach(file => {
    if (file.endsWith('.node') || filesToClean.includes(file)) {
      const filePath = path.join(rootDir, file);
      console.log(`Removing ${file}...`);
      try {
        fs.unlinkSync(filePath);
      } catch (err) {
        console.error(`Failed to remove ${file}:`, err.message);
      }
    }
  });

  console.log('Clean complete.');
}

clean();
