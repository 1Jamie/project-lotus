const fs = require('fs');
const https = require('https');
const http = require('http');
const path = require('path');
const { execSync } = require('child_process');

// Configuration
const REPO = '1Jamie/project-lotus';
const VERSION = 'v' + require('../package.json').version;

function getPlatform() {
    const platform = process.platform;
    const arch = process.arch;

    if (platform === 'win32') {
        return `lotus.win32-${arch}-msvc.node`;
    } else if (platform === 'darwin') {
        return `lotus.darwin-${arch}.node`;
    } else if (platform === 'linux') {
        // Check for openSUSE
        try {
            const osRelease = fs.readFileSync('/etc/os-release', 'utf8').toLowerCase();
            if (osRelease.includes('opensuse') || osRelease.includes('sles')) {
                return `lotus.linux-${arch}-suse.node`;
            }
        } catch (e) {
            // Ignore if /etc/os-release is not readable
        }

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

function isValidBinary(filePath) {
    if (!fs.existsSync(filePath)) return false;
    const stat = fs.statSync(filePath);
    if (stat.size < 1024) return false;

    const fd = fs.openSync(filePath, 'r');
    const header = Buffer.alloc(4);
    fs.readSync(fd, header, 0, 4, 0);
    fs.closeSync(fd);

    // ELF: 0x7f 'E' 'L' 'F'
    const isElf = header[0] === 0x7f && header[1] === 0x45 && header[2] === 0x4c && header[3] === 0x46;
    // PE: 'M' 'Z'
    const isPe = header[0] === 0x4d && header[1] === 0x5a;
    // Mach-O: 0xFEEDFACF / 0xFEEDFACE / 0xCAFEBABE / 0xCFFAEDFE
    const isMacho = (header[0] === 0xcf && header[1] === 0xfa && header[2] === 0xed && header[3] === 0xfe) ||
                    (header[0] === 0xfe && header[1] === 0xed && header[2] === 0xfa && header[3] === 0xcf) ||
                    (header[0] === 0xca && header[1] === 0xfe && header[2] === 0xba && header[3] === 0xbe);

    return isElf || isPe || isMacho;
}

function download(url, dest) {
    const tempDest = `${dest}.tmp.${process.pid}.${Date.now()}`;

    const cleanup = () => {
        try {
            if (fs.existsSync(tempDest)) fs.unlinkSync(tempDest);
        } catch (e) {}
    };

    return new Promise((resolve, reject) => {
        const getClient = (targetUrl) => (targetUrl.startsWith('http:') ? http : https);

        const fetchUrl = (currentUrl, redirectCount = 0) => {
            if (redirectCount > 10) {
                cleanup();
                return reject(new Error('Too many HTTP redirects'));
            }

            const req = getClient(currentUrl).get(currentUrl, (response) => {
                if (response.statusCode >= 300 && response.statusCode < 400 && response.headers.location) {
                    const redirectUrl = new URL(response.headers.location, currentUrl).toString();
                    response.resume(); // Consume stream to free memory
                    return fetchUrl(redirectUrl, redirectCount + 1);
                }

                if (response.statusCode !== 200) {
                    cleanup();
                    response.resume();
                    return reject(new Error(`Failed to download: HTTP ${response.statusCode}`));
                }

                const file = fs.createWriteStream(tempDest);
                response.pipe(file);

                file.on('finish', () => {
                    file.close(() => {
                        try {
                            if (!isValidBinary(tempDest)) {
                                cleanup();
                                return reject(new Error('Downloaded binary failed validation (corrupted or invalid binary header)'));
                            }
                            fs.renameSync(tempDest, dest);
                            resolve();
                        } catch (err) {
                            cleanup();
                            reject(err);
                        }
                    });
                });

                file.on('error', (err) => {
                    cleanup();
                    reject(err);
                });
            });

            req.on('error', (err) => {
                cleanup();
                reject(err);
            });
        };

        fetchUrl(url);
    });
}

async function runInstall({ force = false } = {}) {
    const filename = getPlatform();
    if (!filename) {
        throw new Error(`Unsupported platform: ${process.platform}-${process.arch}`);
    }

    const dest = path.join(__dirname, '..', filename);
    if (fs.existsSync(dest)) {
        if (force) {
            try { fs.unlinkSync(dest); } catch (e) {}
        } else {
            const stat = fs.statSync(dest);
            if (stat.size > 0 && isValidBinary(dest)) {
                console.log('Binary already exists and is valid, skipping download.');
                return;
            }
            console.warn('Existing binary is invalid or 0 bytes, re-downloading...');
            try { fs.unlinkSync(dest); } catch (e) {}
        }
    }

    const url = `https://github.com/${REPO}/releases/download/${VERSION}/${filename}`;
    console.log(`Downloading ${url}...`);

    await download(url, dest);
    console.log('Download complete.');
}

function main() {
    runInstall()
        .catch((err) => {
            console.error('Download failed:', err.message);
            console.error('Run "lotus fetch" to try again.');
        });
}

if (require.main === module) {
    main();
}

module.exports = {
    getPlatform,
    isValidBinary,
    download,
    runInstall
};
