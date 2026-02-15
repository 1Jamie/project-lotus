
const fs = require('fs');
const https = require('https');
const path = require('path');
const { execSync } = require('child_process');

// Configuration
// TODO: Update this to your actual repository
const REPO = 'autumn/lotus';
const VERSION = 'v' + require('../package.json').version;

function getPlatform() {
    const platform = process.platform;
    const arch = process.arch;

    if (platform === 'win32') {
        return `lotus.win32-${arch}-msvc.node`;
    } else if (platform === 'darwin') {
        return `lotus.darwin-${arch}.node`;
    } else if (platform === 'linux') {
        const isMusl = () => {
            try {
                const lddPath = execSync('which ldd').toString().trim();
                return fs.readFileSync(lddPath, 'utf8').includes('musl');
            } catch (e) {
                return true;
            }
        };
        const libc = isMusl() ? 'musl' : 'gnu';
        return `lotus.linux-${arch}-${libc}.node`;
    }
    return null;
}

function download(url, dest) {
    return new Promise((resolve, reject) => {
        const file = fs.createWriteStream(dest);
        const request = https.get(url, (response) => {
            if (response.statusCode === 302 || response.statusCode === 301) {
                download(response.headers.location, dest).then(resolve).catch(reject);
                return;
            }
            if (response.statusCode !== 200) {
                reject(new Error(`Failed to download: ${response.statusCode}`));
                return;
            }
            response.pipe(file);
            file.on('finish', () => {
                file.close(resolve);
            });
        });
        request.on('error', (err) => {
            fs.unlink(dest, () => reject(err));
        });
    });
}

function main() {
    const filename = getPlatform();
    if (!filename) {
        console.error(`Unsupported platform: ${process.platform}-${process.arch}`);
        process.exit(1);
    }

    const dest = path.join(__dirname, '..', filename);
    if (fs.existsSync(dest)) {
        console.log('Binary already exists, skipping download.');
        return;
    }

    const url = `https://github.com/${REPO}/releases/download/${VERSION}/${filename}`;
    console.log(`Downloading ${url}...`);

    download(url, dest)
        .then(() => console.log('Download complete.'))
        .catch((err) => {
            console.error('Download failed:', err.message);
            console.error('Run "lotus fetch" to try again.');
            // Don't fail the build, as we might be in a dev environment or have a fallback
            // actually, for a user install this should logicially fail or warn.
            // The plan says "Fallback: If the download fails, the CLI provides a lotus fetch command to retry."
            // So we exit 0 but warn.
        });
}

main();
