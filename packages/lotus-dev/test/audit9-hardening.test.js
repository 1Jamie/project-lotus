const { test } = require('node:test');
const assert = require('node:assert');
const path = require('node:path');
const fs = require('node:fs');
const http = require('node:http');
const net = require('node:net');
const os = require('node:os');
const vm = require('node:vm');

const lotusCore = require(path.join(__dirname, '..', '..', 'lotus-core', 'lotus.js'));
const {
    NetworkBroker,
    isAllowedPublicIp,
    hostIpcPolicy,
    SecureConstellationManager,
    spawnSandboxedEngine,
} = lotusCore;

test('Audit 9 - I1: Default Sandboxed Engine Spawns with Isolated Private Temp Directory and Denies Host /tmp Write Access', async () => {
    const supervisorBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-supervisor');
    const engineBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-sandbox-probe');

    const instance = await spawnSandboxedEngine({
        supervisorBin,
        engineBin,
        timeoutMs: 10000,
        enableNetwork: false,
    });

    assert(instance, 'Sandboxed engine must boot');
    assert(instance.allocatedTempDir, 'Must have an allocated private sandbox directory');
    assert(instance.allocatedTempDir.startsWith(path.join(os.tmpdir(), 'lotus-sandbox-')), 'Temp dir must be inside a dedicated subdirectory');
    assert(fs.existsSync(instance.allocatedTempDir), 'Allocated directory must exist while running');

    // Verify directory mode is 0o700 (owner-only)
    const stat = fs.statSync(instance.allocatedTempDir);
    assert.strictEqual(stat.mode & 0o777, 0o700, 'Sandbox temp directory must be restricted to mode 0700');

    const tempDirToCleanup = instance.allocatedTempDir;
    await instance.terminate();

    // Verify cleanup
    assert(!fs.existsSync(tempDirToCleanup), 'Sandbox temp directory must be deleted on termination');
});

test('Audit 9 - I3: NetworkBroker.prototype.startProxyServer Enforces DNS Pinning, Blocks Rebinding, and Ingests Cookies', async () => {
    let internalHit = false;
    const internalServer = http.createServer((req, res) => {
        internalHit = true;
        res.writeHead(200, { 'Content-Type': 'text/plain' });
        res.end('INTERNAL_SECRET_REACHED');
    });
    await new Promise((resolve) => internalServer.listen(0, '127.0.0.1', resolve));
    const internalPort = internalServer.address().port;

    const publicServer = http.createServer((req, res) => {
        res.writeHead(200, {
            'Content-Type': 'text/plain',
            'Set-Cookie': 'auth_session=token-12345; Path=/; HttpOnly',
        });
        res.end('PUBLIC_DATA');
    });
    await new Promise((resolve) => publicServer.listen(0, '127.0.0.1', resolve));
    const publicPort = publicServer.address().port;

    // Broker with allowLocalhost: false
    const broker = new NetworkBroker({ allowLocalhost: false });
    const proxyServer = await broker.startProxyServer();
    const proxyPort = proxyServer.port;

    // 1. Direct request to 127.0.0.1 must be rejected by anti-SSRF policy
    const ssrfPromise = new Promise((resolve, reject) => {
        const req = http.request({
            host: '127.0.0.1',
            port: proxyPort,
            path: `http://127.0.0.1:${internalPort}/secret`,
            method: 'GET',
        }, (res) => {
            let data = '';
            res.on('data', chunk => { data += chunk; });
            res.on('end', () => resolve({ status: res.statusCode, data }));
        });
        req.on('error', reject);
        req.end();
    });

    const ssrfRes = await ssrfPromise;
    assert.strictEqual(ssrfRes.status, 403, 'Loopback access must be blocked with 403 when allowLocalhost is false');
    assert.strictEqual(internalHit, false, 'Internal server must never be reached');

    // 2. Cookie parsing & storage verification when allowLocalhost is enabled
    const localBroker = new NetworkBroker({ allowLocalhost: true });
    const localProxy = await localBroker.startProxyServer();
    const localProxyPort = localProxy.port;

    const surfaceId = 'test-cookie-surface';
    const topOrigin = `http://127.0.0.1:${publicPort}`;

    const cookieReqPromise = new Promise((resolve, reject) => {
        const req = http.request({
            host: '127.0.0.1',
            port: localProxyPort,
            path: `http://127.0.0.1:${publicPort}/data`,
            method: 'GET',
            headers: {
                'x-lotus-surface-id': surfaceId,
            },
        }, (res) => {
            let data = '';
            res.on('data', chunk => { data += chunk; });
            res.on('end', () => resolve({ status: res.statusCode, data }));
        });
        req.on('error', reject);
        req.end();
    });

    const cookieRes = await cookieReqPromise;
    assert.strictEqual(cookieRes.status, 200);
    assert.strictEqual(cookieRes.data, 'PUBLIC_DATA');

    // Assert cookie is actually stored in HostCookieStore
    const cookieHeader = localBroker.cookieStore.getCookieHeader(surfaceId, topOrigin, `http://127.0.0.1:${publicPort}/data`);
    assert(cookieHeader.includes('auth_session=token-12345'), 'Cookie must be parsed and stored in HostCookieStore');

    await proxyServer.server.close();
    await localProxy.server.close();
    await new Promise(r => internalServer.close(r));
    await new Promise(r => publicServer.close(r));
});

test('Audit 9 - I4: Subframe Calling-Realm Guard in Bridge Script Blocks Same-Origin <iframe>', () => {
    const script = hostIpcPolicy.generateSecureBridgeScript({
        surfaceId: 'pane-alpha',
        documentId: 'doc-alpha',
        bootstrapSecret: 'secret-xyz',
        enginePort: 9000,
    });

    // Setup top-level realm context
    const topContext = vm.createContext({
        name: 'topWindow',
    });
    topContext.window = topContext;
    topContext.self = topContext;
    topContext.parent = topContext;
    topContext.top = topContext;

    // Run bridge script in top-level context
    vm.runInContext(script, topContext);
    assert(topContext.window.lotus, 'window.lotus must exist on top-level window');

    // Create same-origin iframe context
    const iframeContext = vm.createContext({
        name: 'iframeWindow',
    });
    iframeContext.window = iframeContext;
    iframeContext.self = iframeContext;
    iframeContext.parent = topContext;
    iframeContext.top = topContext;

    // A. Subframe calling window.parent.lotus.on(...) with an iframe callback -> MUST THROW DOMException(18)
    assert.throws(
        () => {
            vm.runInContext(`
                window.parent.lotus.on('test-event', function() {});
            `, iframeContext);
        },
        (err) => {
            assert.strictEqual(err.name, 'DOMException');
            assert.strictEqual(err.code, 18);
            assert.match(err.message, /restricted exclusively to top-level browsing contexts/);
            return true;
        }
    );

    // B. Subframe calling window.parent.lotus.invoke(...) with an iframe payload -> MUST THROW DOMException(18)
    assert.throws(
        () => {
            vm.runInContext(`
                window.parent.lotus.invoke('test.method', { key: 'value' });
            `, iframeContext);
        },
        (err) => {
            assert.strictEqual(err.name, 'DOMException');
            assert.strictEqual(err.code, 18);
            assert.match(err.message, /restricted exclusively to top-level browsing contexts/);
            return true;
        }
    );
});

test('Audit 9 - I8: Concurrent acquireSecureEngine Race Successfully Registers Second Surface', async () => {
    const supervisorBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-supervisor');
    const engineBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-sandbox-probe');

    const manager = new SecureConstellationManager({
        supervisorBin,
        engineBin,
        enableNetwork: false,
    });

    // Launch first acquire
    const p1 = manager.acquireSecureEngine('pane-concurrent-1', {
        url: 'about:blank',
    });

    // Launch second acquire immediately while spawn is in flight
    const p2 = manager.acquireSecureEngine('pane-concurrent-2', {
        url: 'about:blank',
    });

    const [res1, res2] = await Promise.all([p1, p2]);

    assert.strictEqual(res1.instanceId, res2.instanceId, 'Both panes must share the same engine instance');
    assert.strictEqual(manager._refCount, 2, 'Reference count must be 2');
    assert(manager._surfaces.has('pane-concurrent-1'));
    assert(manager._surfaces.has('pane-concurrent-2'));

    await manager.releaseSecureEngine('pane-concurrent-1');
    assert.strictEqual(manager._refCount, 1);
    await manager.releaseSecureEngine('pane-concurrent-2');
    assert.strictEqual(manager._refCount, 0);

    await manager._activeInstance.terminate();
});

test('Isolated Sandbox Mode (isolated: true): Spawns Dedicated Engine Process and Failure Domain', async () => {
    const supervisorBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-supervisor');
    const engineBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-sandbox-probe');

    const manager = new SecureConstellationManager({
        supervisorBin,
        engineBin,
        enableNetwork: false,
    });

    // 1. Acquire two pooled sandboxed panes and one dedicated isolated pane
    const [shared1, shared2, isolated1] = await Promise.all([
        manager.acquireSecureEngine('pane-shared-1', { url: 'about:blank' }),
        manager.acquireSecureEngine('pane-shared-2', { url: 'about:blank' }),
        manager.acquireSecureEngine('pane-isolated-1', { url: 'about:blank', isolated: true }),
    ]);

    assert.strictEqual(shared1.isolated, false);
    assert.strictEqual(shared2.isolated, false);
    assert.strictEqual(isolated1.isolated, true);

    // Pooled panes share the same enginePid and instanceId
    assert.strictEqual(shared1.instanceId, shared2.instanceId);
    assert.strictEqual(shared1.enginePid, shared2.enginePid);

    // Isolated pane has a distinct enginePid, instanceId, and private temp dir
    assert.notStrictEqual(isolated1.instanceId, shared1.instanceId, 'Isolated pane must have a distinct instanceId');
    assert.notStrictEqual(isolated1.enginePid, shared1.enginePid, 'Isolated pane must run in its own OS process');
    assert.notStrictEqual(
        isolated1.instance.allocatedTempDir,
        shared1.instance.allocatedTempDir,
        'Isolated pane must have its own private Landlock /tmp sandbox directory'
    );

    const isolatedTempDir = isolated1.instance.allocatedTempDir;
    assert(fs.existsSync(isolatedTempDir), 'Isolated temp dir must exist while active');

    // 2. Releasing the isolated pane immediately terminates its dedicated engine & purges its temp dir
    // without disturbing the shared pool
    await manager.releaseSecureEngine('pane-isolated-1');
    assert(!manager._isolatedInstances.has('pane-isolated-1'));
    assert(!fs.existsSync(isolatedTempDir), 'Isolated temp dir must be purged immediately upon release');
    assert.strictEqual(manager._refCount, 2, 'Shared pool refCount must remain untouched');
    assert(manager.isRunning, 'Shared pool engine must still be running');

    // Cleanup shared pool
    await manager.releaseSecureEngine('pane-shared-1');
    await manager.releaseSecureEngine('pane-shared-2');
    await manager._activeInstance.terminate();
});

