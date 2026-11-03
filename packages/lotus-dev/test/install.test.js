const test = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const http = require('http');
const path = require('path');
const os = require('os');
const { isValidBinary, download } = require('../../lotus-core/scripts/install.js');

test('Install - isValidBinary identifies valid headers and rejects empty/malformed files', () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'lotus-inst-val-'));
    try {
        const emptyFile = path.join(tmpDir, 'empty.bin');
        fs.writeFileSync(emptyFile, Buffer.alloc(0));
        assert.strictEqual(isValidBinary(emptyFile), false);

        const smallFile = path.join(tmpDir, 'small.bin');
        fs.writeFileSync(smallFile, Buffer.from('too small'));
        assert.strictEqual(isValidBinary(smallFile), false);

        const elfFile = path.join(tmpDir, 'test.node');
        const elfBuf = Buffer.alloc(2048);
        elfBuf[0] = 0x7f; elfBuf[1] = 0x45; elfBuf[2] = 0x4c; elfBuf[3] = 0x46; // \x7fELF
        fs.writeFileSync(elfFile, elfBuf);
        assert.strictEqual(isValidBinary(elfFile), true);

        const peFile = path.join(tmpDir, 'test.exe');
        const peBuf = Buffer.alloc(2048);
        peBuf[0] = 0x4d; peBuf[1] = 0x5a; // MZ
        fs.writeFileSync(peFile, peBuf);
        assert.strictEqual(isValidBinary(peFile), true);
    } finally {
        fs.rmSync(tmpDir, { recursive: true, force: true });
    }
});

test('Install - download handles errors atomically and leaves no invalid files on failure', async () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'lotus-inst-down-'));

    // Create a local test server
    const server = http.createServer((req, res) => {
        if (req.url === '/redirect') {
            res.writeHead(302, { Location: '/valid' });
            res.end();
        } else if (req.url === '/valid') {
            res.writeHead(200, { 'Content-Type': 'application/octet-stream' });
            const validElf = Buffer.alloc(2048);
            validElf[0] = 0x7f; validElf[1] = 0x45; validElf[2] = 0x4c; validElf[3] = 0x46;
            res.end(validElf);
        } else if (req.url === '/fail') {
            res.writeHead(500);
            res.end('Server Error');
        } else if (req.url === '/corrupt') {
            res.writeHead(200);
            res.end('Not a valid binary file');
        }
    });

    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
    const port = server.address().port;

    try {
        // Test 1: Non-200 failure must clean up and leave no dest file
        const destFail = path.join(tmpDir, 'should-not-exist.node');
        await assert.rejects(
            async () => download(`http://127.0.0.1:${port}/fail`, destFail),
            /HTTP 500/
        );
        assert.strictEqual(fs.existsSync(destFail), false);

        // Test 2: Validation failure on corrupted binary must clean up
        const destCorrupt = path.join(tmpDir, 'corrupt.node');
        await assert.rejects(
            async () => download(`http://127.0.0.1:${port}/corrupt`, destCorrupt),
            /validation/
        );
        assert.strictEqual(fs.existsSync(destCorrupt), false);

        // Test 3: Successful download through redirect
        const destValid = path.join(tmpDir, 'valid.node');
        await download(`http://127.0.0.1:${port}/redirect`, destValid);
        assert.strictEqual(fs.existsSync(destValid), true);
        assert.strictEqual(isValidBinary(destValid), true);
    } finally {
        server.close();
        fs.rmSync(tmpDir, { recursive: true, force: true });
    }
});

