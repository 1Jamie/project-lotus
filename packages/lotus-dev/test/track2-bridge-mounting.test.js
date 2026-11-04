const { test } = require('node:test');
const assert = require('node:assert');
const path = require('node:path');
const http = require('node:http');

const lotusCore = require(path.join(__dirname, '..', '..', 'lotus-core', 'lotus.js'));
const {
    ipcMain,
    hostIpcRouter,
    hostIpcPolicy,
    getIpcPort,
    warmup,
    registerSurfacePolicy,
    dispatchToSurface
} = lotusCore;

function requestHttp({ port, path, method = 'POST', headers = {}, body = '' }) {
    return new Promise((resolve, reject) => {
        const req = http.request({
            host: '127.0.0.1',
            port,
            path,
            method,
            headers
        }, (res) => {
            let data = '';
            res.on('data', chunk => { data += chunk; });
            res.on('end', () => resolve({ status: res.statusCode, body: data }));
        });
        req.on('error', reject);
        if (body) req.write(body);
        req.end();
    });
}

test('Step 4: Track 2 Mounting & Bridge Wiring - /ipc/handshake and /ipc/secure-ws verification (Finding R4)', async () => {
    // 1. Initialize native core and IPC server
    warmup();

    let port = getIpcPort();
    if (!port) {
        port = await new Promise((resolve) => {
            const timeout = setTimeout(() => resolve(getIpcPort()), 3000);
            ipcMain.once('ready', (msg) => {
                clearTimeout(timeout);
                resolve(msg.ipc_port);
            });
        });
    }

    assert(port > 0, `IPC server port must be valid, got ${port}`);

    // 2. Create and register a secure surface
    const surfaceId = 'test-track2-secure-pane';
    const documentId = 'doc-track2-auth-100';
    const bootstrapSecret = 'test-sec-bootstrap-xyz-987';

    const surface = hostIpcRouter.surfaceTracker.createSurface(surfaceId, {
        secure: true,
        documentId,
        bootstrapSecret,
        allowedChannels: ['vault'],
        origin: 'http://127.0.0.1:' + port,
    });

    assert(surface, 'Surface should be created');
    assert.strictEqual(surface.secure, true);

    // 3. Test /ipc/handshake security invariants
    // A. Rejects invalid Host header (DNS rebinding / SSRF protection)
    const badHostResp = await requestHttp({
        port,
        path: '/ipc/handshake',
        method: 'POST',
        headers: {
            'Host': `evil.example.com:${port}`,
            'Content-Type': 'application/json',
            'X-Lotus-Bootstrap': bootstrapSecret
        },
        body: JSON.stringify({ surfaceId, documentId })
    });
    assert.strictEqual(badHostResp.status, 403, 'Invalid Host header must be rejected with 403 Forbidden');

    // B. Rejects missing X-Lotus-Bootstrap header
    const noSecretResp = await requestHttp({
        port,
        path: '/ipc/handshake',
        method: 'POST',
        headers: {
            'Host': `127.0.0.1:${port}`,
            'Content-Type': 'application/json'
        },
        body: JSON.stringify({ surfaceId, documentId })
    });
    assert.strictEqual(noSecretResp.status, 401, 'Missing bootstrap header must be rejected with 401 Unauthorized');

    // C. Rejects invalid bootstrap secret
    const wrongSecretResp = await requestHttp({
        port,
        path: '/ipc/handshake',
        method: 'POST',
        headers: {
            'Host': `127.0.0.1:${port}`,
            'Content-Type': 'application/json',
            'X-Lotus-Bootstrap': 'wrong-attacker-secret'
        },
        body: JSON.stringify({ surfaceId, documentId })
    });
    assert.strictEqual(wrongSecretResp.status, 401, 'Wrong bootstrap secret must be rejected with 401 Unauthorized');

    // D. Rejects unknown surfaceId
    const unknownSurfResp = await requestHttp({
        port,
        path: '/ipc/handshake',
        method: 'POST',
        headers: {
            'Host': `127.0.0.1:${port}`,
            'Content-Type': 'application/json',
            'X-Lotus-Bootstrap': bootstrapSecret
        },
        body: JSON.stringify({ surfaceId: 'unregistered-surface-id', documentId })
    });
    assert.strictEqual(unknownSurfResp.status, 404, 'Unknown surfaceId must be rejected with 404 Not Found');

    // E. Rejects mismatched documentId
    const wrongDocResp = await requestHttp({
        port,
        path: '/ipc/handshake',
        method: 'POST',
        headers: {
            'Host': `127.0.0.1:${port}`,
            'Content-Type': 'application/json',
            'X-Lotus-Bootstrap': bootstrapSecret
        },
        body: JSON.stringify({ surfaceId, documentId: 'wrong-stale-doc-id' })
    });
    assert.strictEqual(wrongDocResp.status, 409, 'Mismatched documentId must be rejected with 409 Conflict');

    // F. Legitimate handshake succeeds with single-use ticket
    const validHandshakeResp = await requestHttp({
        port,
        path: '/ipc/handshake',
        method: 'POST',
        headers: {
            'Host': `127.0.0.1:${port}`,
            'Content-Type': 'application/json',
            'X-Lotus-Bootstrap': bootstrapSecret
        },
        body: JSON.stringify({ surfaceId, documentId })
    });
    assert.strictEqual(validHandshakeResp.status, 200, 'Legitimate handshake must return 200 OK');
    const handshakeData = JSON.parse(validHandshakeResp.body);
    assert(handshakeData.ticket, 'Handshake must return a ticket string');
    assert.strictEqual(handshakeData.ticket.length, 64, 'Ticket must be a 256-bit hex string (64 characters)');
    assert.strictEqual(handshakeData.expiresIn, 30, 'Ticket TTL must be 30 seconds');

    const validTicket = handshakeData.ticket;

    // 4. Test /ipc/secure-ws WebSocket upgrade & atomic ticket consumption
    // A. Connect with forged ticket -> rejected
    const badTicketWsUrl = `ws://127.0.0.1:${port}/ipc/secure-ws?surfaceId=${encodeURIComponent(surfaceId)}&documentId=${encodeURIComponent(documentId)}&ticket=forged-ticket-12345`;
    const badWsPromise = new Promise((resolve) => {
        const badWs = new WebSocket(badTicketWsUrl);
        badWs.onopen = () => {
            badWs.close();
            resolve('OPEN');
        };
        badWs.onerror = () => {
            resolve('ERROR');
        };
    });
    const badWsResult = await badWsPromise;
    assert.strictEqual(badWsResult, 'ERROR', 'WebSocket connection with forged ticket must be rejected');

    // B. Connect with genuine ticket -> succeeds (N2: bad attempt above did not burn genuine ticket)
    const validWsUrl = `ws://127.0.0.1:${port}/ipc/secure-ws?surfaceId=${encodeURIComponent(surfaceId)}&documentId=${encodeURIComponent(documentId)}&ticket=${encodeURIComponent(validTicket)}`;
    const ws = new WebSocket(validWsUrl);

    await new Promise((resolve, reject) => {
        ws.onopen = () => resolve();
        ws.onerror = (e) => reject(new Error('WebSocket connection failed'));
    });
    assert.strictEqual(ws.readyState, WebSocket.OPEN, 'Secure WebSocket must be OPEN');

    // C. Replay attempt with same ticket -> rejected (single-use ticket consumed)
    const replayWsPromise = new Promise((resolve) => {
        const replayWs = new WebSocket(validWsUrl);
        replayWs.onopen = () => {
            replayWs.close();
            resolve('OPEN');
        };
        replayWs.onerror = () => {
            resolve('ERROR');
        };
    });
    const replayResult = await replayWsPromise;
    assert.strictEqual(replayResult, 'ERROR', 'Replayed ticket must be rejected');

    // 5. Test Track 2 method invocation and output projection
    const unregisterMethod = ipcMain.register('vault.getSecretKey', async (payload) => {
        return {
            keyId: payload.keyId,
            secretValue: 'super-secret-vault-content',
            internalTag: 'MUST_BE_DROPPED_BY_SCHEMA_PROJECTION'
        };
    }, {
        channel: 'vault',
        schema: {
            keyId: 'string',
            secretValue: 'string'
        }
    });

    const messageQueue = [];
    let messageWaiter = null;
    ws.onmessage = (event) => {
        const parsed = JSON.parse(event.data);
        if (messageWaiter) {
            const w = messageWaiter;
            messageWaiter = null;
            w(parsed);
        } else {
            messageQueue.push(parsed);
        }
    };

    function waitForNextMessage(timeoutMs = 3000) {
        if (messageQueue.length > 0) {
            return Promise.resolve(messageQueue.shift());
        }
        return new Promise((resolve, reject) => {
            const timer = setTimeout(() => reject(new Error('Timed out waiting for WS message')), timeoutMs);
            messageWaiter = (msg) => {
                clearTimeout(timer);
                resolve(msg);
            };
        });
    }

    // Send Track 2 invoke request over WebSocket
    ws.send(JSON.stringify({
        type: 'invoke',
        id: 42,
        method: 'vault.getSecretKey',
        payload: { keyId: 'key-alpha-99' },
        documentId
    }));

    const replyMsg = await waitForNextMessage();
    assert.strictEqual(replyMsg.type, 'reply');
    assert.strictEqual(replyMsg.id, 42);
    assert.strictEqual(replyMsg.result.keyId, 'key-alpha-99');
    assert.strictEqual(replyMsg.result.secretValue, 'super-secret-vault-content');
    assert.strictEqual(replyMsg.result.internalTag, undefined, 'Output projection must strip undeclared fields');

    // Clean up
    unregisterMethod();
    ws.close();

    // 6. Verify Finding R4: Subframe & Incumbent Realm Guard in Bridge Script
    const script = hostIpcPolicy.generateSecureBridgeScript({
        surfaceId,
        documentId,
        bootstrapSecret,
        enginePort: port
    });

    // A. Generated script explicitly guards top-level browsing context
    assert.match(script, /window\.parent !== window \|\| window\.top !== window/);
    assert.match(script, /verifyTopRealm\(\)/);
    assert.match(script, /SecurityError: Lotus IPC is restricted exclusively to top-level browsing contexts/);

    // B. Subframe simulation: when window.parent !== window, executing bridge refuses access
    const mockSubframeWindow = {
        top: {},
        parent: {},
    };
    mockSubframeWindow.self = mockSubframeWindow;
    mockSubframeWindow.window = mockSubframeWindow;

    const runInSubframe = new Function('window', script);
    runInSubframe(mockSubframeWindow);

    // Window.lotus must NOT be created in a subframe
    assert.strictEqual(mockSubframeWindow.lotus, undefined, 'Subframe must be denied window.lotus bridge entirely');

    setTimeout(() => {
        lotusCore.exit(0);
    }, 50);
});
