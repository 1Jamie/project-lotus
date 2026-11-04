const { test } = require('node:test');
const assert = require('node:assert');
const path = require('node:path');
const fs = require('node:fs');
const http = require('node:http');

const lotusCore = require(path.join(__dirname, '..', '..', 'lotus-core', 'lotus.js'));
const { spawnSandboxedEngine, allocateRemotePaneShm, NetworkBroker, isAllowedPublicIp } = lotusCore;

const supervisorBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-supervisor');
const engineBin = path.resolve(__dirname, '../../lotus-engine/target/debug/lotus-engine');

test('Step 3: Network Broker - anti-SSRF proxy routing for secure pane loading HTTP page', async () => {
    if (process.platform !== 'linux') {
        return; // Linux-only test
    }

    assert(fs.existsSync(supervisorBin), `Supervisor binary must exist at ${supervisorBin}`);
    assert(fs.existsSync(engineBin), `Engine binary must exist at ${engineBin}`);

    // 1. Verify Anti-SSRF classification unit invariants
    assert.strictEqual(isAllowedPublicIp('127.0.0.1', false), false, '127.0.0.1 blocked by default');
    assert.strictEqual(isAllowedPublicIp('127.0.0.1', true), true, '127.0.0.1 allowed when allowLocalhost: true');
    assert.strictEqual(isAllowedPublicIp('10.0.0.1', true), false, 'RFC1918 10.0.0.0/8 blocked even with allowLocalhost');
    assert.strictEqual(isAllowedPublicIp('192.168.1.1', true), false, 'RFC1918 192.168.0.0/16 blocked');
    assert.strictEqual(isAllowedPublicIp('169.254.169.254', true), false, 'Cloud metadata IP blocked');
    assert.strictEqual(isAllowedPublicIp('8.8.8.8', false), true, 'Public IP 8.8.8.8 allowed');

    // 2. Start a test HTTP server on localhost
    let requestReceived = false;
    let requestedUrl = null;
    const testHtml = '<!DOCTYPE html><html><head><title>Broker Success</title></head><body style="background:#00ff00;"><h1>Secure Pane Online</h1></body></html>';

    const testServer = http.createServer((req, res) => {
        requestReceived = true;
        requestedUrl = req.url;
        res.writeHead(200, {
            'Content-Type': 'text/html; charset=utf-8',
            'Content-Length': Buffer.byteLength(testHtml),
            'Set-Cookie': 'lotus_broker_test=verified; Path=/; HttpOnly',
        });
        res.end(testHtml);
    });

    await new Promise((resolve) => testServer.listen(0, '127.0.0.1', resolve));
    const serverPort = testServer.address().port;
    const targetUrl = `http://127.0.0.1:${serverPort}/test-broker`;

    // 3. Pre-allocate Shared Memory for the test pane
    const width = 400;
    const height = 300;
    const surfaceId = 'test-win:network-pane-1';
    const shm = allocateRemotePaneShm(surfaceId, width, height);

    // 4. Create NetworkBroker with localhost allowed for test server
    const broker = new NetworkBroker({ allowLocalhost: true });

    // 5. Spawn sandboxed engine wired to NetworkBroker
    const engineInstance = await spawnSandboxedEngine({
        supervisorBin,
        engineBin,
        timeoutMs: 15000,
        networkBroker: broker,
        allowLocalhost: true,
        passFds: [shm.rwFd, shm.ackRoFd],
        engineArgs: [
            '--shm-fd', shm.rwFd.toString(),
            '--ack-fd', shm.ackRoFd.toString(),
            '--surface-id', surfaceId,
            '--width', width.toString(),
            '--height', height.toString(),
            '--url', targetUrl,
        ],
    });
    assert(engineInstance, 'Engine instance must be created');
    assert(engineInstance.brokerServer, 'Engine must have active brokerServer proxy');

    try {
        // 6. Monitor engine events for LoadStatus matching targetUrl
        const loadStatusPromise = new Promise((resolve, reject) => {
            const timeout = setTimeout(() => reject(new Error('Timed out waiting for LoadStatus event for ' + targetUrl)), 15000);
            let buffer = '';

            const onData = (chunk) => {
                buffer += chunk.toString('utf8');
                const lines = buffer.split('\n');
                buffer = lines.pop();

                for (const line of lines) {
                    if (!line.trim()) continue;
                    try {
                        const msg = JSON.parse(line.trim());
                        if (msg.type === 'LoadStatus' && msg.url === targetUrl) {
                            clearTimeout(timeout);
                            engineInstance.controlStream.removeListener('data', onData);
                            resolve(msg);
                            return;
                        }
                    } catch {}
                }
            };

            engineInstance.controlStream.on('data', onData);
        });

        const loadStatus = await loadStatusPromise;
        assert(loadStatus, 'LoadStatus must be received');
        assert.strictEqual(loadStatus.surface_id, surfaceId, 'LoadStatus surface_id must match');
        assert.strictEqual(loadStatus.status, 'complete', `LoadStatus must be complete: ${JSON.stringify(loadStatus)}`);
        assert.strictEqual(loadStatus.url, targetUrl, `LoadStatus URL must match target URL`);

        // 7. Verify test HTTP server handled the request
        assert.strictEqual(requestReceived, true, 'Test HTTP server must have received request via proxy');
        assert.strictEqual(requestedUrl, '/test-broker', 'Requested path must match');

        // 8. Verify anti-SSRF enforcement on broker proxy server:
        // Connecting to RFC 1918 private IP via the proxy must be rejected with 403 Forbidden
        await new Promise((resolve, reject) => {
            const httpReq = http.request({
                socketPath: engineInstance.brokerServer.socketPath,
                path: 'http://10.0.0.1/forbidden',
                method: 'GET',
            }, (res) => {
                res.resume();
                try {
                    assert.strictEqual(res.statusCode, 403, 'Proxy must reject SSRF request to 10.0.0.1 with 403 Forbidden');
                    resolve();
                } catch (err) {
                    reject(err);
                }
            });
            httpReq.on('error', reject);
            httpReq.end();
        });

    } finally {
        // Clean up
        await engineInstance.terminate();
        await new Promise((resolve) => testServer.close(resolve));
    }
});
