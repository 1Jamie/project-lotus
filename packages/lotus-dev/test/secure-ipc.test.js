const test = require('node:test');
const assert = require('node:assert');
const util = require('node:util');
const {
    readSafeDataProperty,
    validateStructuralBounds,
    sanitizeClone,
    validateAndProjectOutput,
    HostSurfaceTracker,
    HostIpcPolicyRouter,
    generateSecureBridgeScript
} = require('@lotus-gui/core').hostIpcPolicy;

test('Track 1 Inbound - verifies origin, documentId, navigation state and channel isolation', () => {
    const tracker = new HostSurfaceTracker();
    const router = new HostIpcPolicyRouter(tracker);

    const surface = tracker.createSurface('win-1', {
        secure: false,
        documentId: 'doc-1',
        origin: 'http://127.0.0.1:3000',
        trustedOrigins: ['http://127.0.0.1:3000'],
        maxPayloadBytes: 1024
    });

    router.handleTrack1('greet', (data) => `Hello ${data.name}`);

    // 1. Valid frame succeeds
    const frame = { documentId: 'doc-1' };
    assert.strictEqual(router.validateTrack1Inbound(surface, frame, 50, false), true);

    // 2. Reject if arriving on LOTUS_CONTROL_FD_SECURE
    assert.throws(() => {
        router.validateTrack1Inbound(surface, frame, 50, true);
    }, { code: 'E_WRONG_CHANNEL' });

    // 3. Reject if payload exceeds maxPayloadBytes
    assert.throws(() => {
        router.validateTrack1Inbound(surface, frame, 2048, false);
    }, { code: 'E_PAYLOAD_TOO_LARGE' });

    // 4. Reject if surface is secure: true
    surface.secure = true;
    assert.throws(() => {
        router.validateTrack1Inbound(surface, frame, 50, false);
    }, { code: 'E_SURFACE_SECURE_MISMATCH' });
    surface.secure = false;

    // 5. Reject if navigation is provisional or failed
    surface.navigationState = 'provisional';
    assert.throws(() => {
        router.validateTrack1Inbound(surface, frame, 50, false);
    }, { code: 'E_NAVIGATION_NOT_COMMITTED' });
    surface.navigationState = 'committed';

    // 6. Reject if documentId does not match
    assert.throws(() => {
        router.validateTrack1Inbound(surface, { documentId: 'stale-doc' }, 50, false);
    }, { code: 'E_DOCUMENT_MISMATCH' });

    // 7. Reject if origin is not trusted
    surface.currentOrigin = 'http://attacker.example.com';
    assert.throws(() => {
        router.validateTrack1Inbound(surface, frame, 50, false);
    }, { code: 'E_ORIGIN_NOT_TRUSTED' });
});

test('Track 1 Post-Await Validation - drops reply on navigation, unregister, or generation bump', async () => {
    const tracker = new HostSurfaceTracker();
    const router = new HostIpcPolicyRouter(tracker);

    const surface = tracker.createSurface('win-1', {
        secure: false,
        documentId: 'doc-1',
        origin: 'http://127.0.0.1:3000'
    });

    const unreg = router.handleTrack1('compute', async () => 'done');
    const entry = router.track1Handlers.get('compute');

    // Valid pre-conditions
    assert.strictEqual(
        router.validateTrack1PostAwait(surface, entry, 'doc-1', entry.generationId),
        true
    );

    // 1. Navigation advances activeDocumentId -> dropped
    surface.activeDocumentId = 'doc-2';
    assert.strictEqual(
        router.validateTrack1PostAwait(surface, entry, 'doc-1', entry.generationId),
        false
    );
    surface.activeDocumentId = 'doc-1';

    // 2. Navigation state changed to provisional -> dropped
    surface.navigationState = 'provisional';
    assert.strictEqual(
        router.validateTrack1PostAwait(surface, entry, 'doc-1', entry.generationId),
        false
    );
    surface.navigationState = 'committed';

    // 3. Handler was re-registered (bumping generationId) -> old post-await dropped
    router.handleTrack1('compute', async () => 'new handler');
    assert.strictEqual(
        router.validateTrack1PostAwait(surface, entry, 'doc-1', entry.generationId),
        false
    );

    // 4. Handler unregistered -> dropped
    unreg();
    assert.strictEqual(
        router.validateTrack1PostAwait(surface, entry, 'doc-1', entry.generationId),
        false
    );
});

test('Track 1 Outbound - stamps documentId and rejects insecure or uncommitted states', () => {
    const tracker = new HostSurfaceTracker();
    const router = new HostIpcPolicyRouter(tracker);

    const surface = tracker.createSurface('win-1', {
        secure: false,
        documentId: 'doc-active'
    });

    const out = router.validateTrack1Outbound(surface, 'msg', { text: 'hello' });
    assert.strictEqual(out.documentId, 'doc-active');
    assert.deepStrictEqual(out.data, { text: 'hello' });

    // Rejects secure surfaces
    surface.secure = true;
    assert.throws(() => router.validateTrack1Outbound(surface, 'msg', {}), /E_SURFACE_SECURE_MISMATCH/);
    surface.secure = false;

    // Rejects provisional navigation
    surface.navigationState = 'provisional';
    assert.throws(() => router.validateTrack1Outbound(surface, 'msg', {}), /E_NAVIGATION_NOT_COMMITTED/);
    surface.navigationState = 'committed';

    // Rejects ipcEligible = false
    surface.ipcEligible = false;
    assert.throws(() => router.validateTrack1Outbound(surface, 'msg', {}), /E_IPC_INELIGIBLE/);
});

test('Track 2 Inbound - enforces secure: true, channels, methods and options schema', () => {
    const tracker = new HostSurfaceTracker();
    const router = new HostIpcPolicyRouter(tracker);

    const surface = tracker.createSurface('pane-sec', {
        secure: true,
        documentId: 'doc-sec-1',
        allowedChannels: ['vault']
    });

    // Options are mandatory
    assert.throws(() => {
        router.registerSecureMethod('secretRead', () => {});
    }, /mandatory options object/);

    router.registerSecureMethod('secretRead', () => ({ secret: 42 }), {
        channel: 'vault'
    });

    router.registerSecureMethod('adminNuke', () => {}, {
        channel: 'admin'
    });

    // 1. Authorized call on granted channel 'vault' succeeds
    const regEntry = router.validateTrack2Inbound(surface, {
        method: 'secretRead',
        documentId: 'doc-sec-1'
    }, 100);
    assert.strictEqual(regEntry.method, 'secretRead');
    assert.strictEqual(regEntry.channel, 'vault');

    // 2. Denied call on unauthorized channel 'admin'
    assert.throws(() => {
        router.validateTrack2Inbound(surface, {
            method: 'adminNuke',
            documentId: 'doc-sec-1'
        }, 100);
    }, { code: 'E_CHANNEL_FORBIDDEN' });

    // 3. Dynamically grant channel 'admin' -> now succeeds
    tracker.grantChannel('pane-sec', 'admin');
    const adminEntry = router.validateTrack2Inbound(surface, {
        method: 'adminNuke',
        documentId: 'doc-sec-1'
    }, 100);
    assert.strictEqual(adminEntry.channel, 'admin');

    // 4. Revoke channel 'admin' -> fails again
    tracker.revokeChannel('pane-sec', 'admin');
    assert.throws(() => {
        router.validateTrack2Inbound(surface, {
            method: 'adminNuke',
            documentId: 'doc-sec-1'
        }, 100);
    }, { code: 'E_CHANNEL_FORBIDDEN' });
});

test('Track 2 Post-Await Validation - verifies channel, generationId and documentId across async intervals', () => {
    const tracker = new HostSurfaceTracker();
    const router = new HostIpcPolicyRouter(tracker);

    const surface = tracker.createSurface('pane-sec', {
        secure: true,
        documentId: 'doc-sec-1',
        allowedChannels: ['crypto']
    });

    router.registerSecureMethod('sign', async () => 'sig', { channel: 'crypto' });
    const regEntry = router.track2Methods.get('sign');

    assert.strictEqual(
        router.validateTrack2PostAwait(surface, regEntry, 'doc-sec-1', regEntry.generationId),
        true
    );

    // Channel revoked during await -> dropped
    tracker.revokeChannel('pane-sec', 'crypto');
    assert.strictEqual(
        router.validateTrack2PostAwait(surface, regEntry, 'doc-sec-1', regEntry.generationId),
        false
    );
    tracker.grantChannel('pane-sec', 'crypto');

    // Document navigated during await -> dropped
    surface.activeDocumentId = 'doc-sec-2';
    assert.strictEqual(
        router.validateTrack2PostAwait(surface, regEntry, 'doc-sec-1', regEntry.generationId),
        false
    );
    surface.activeDocumentId = 'doc-sec-1';

    // Method unregistered during await -> dropped
    router.unregisterSecureMethod('sign');
    assert.strictEqual(
        router.validateTrack2PostAwait(surface, regEntry, 'doc-sec-1', regEntry.generationId),
        false
    );
});

test('Proxy and Accessor Safety - util.types.isProxy checked FIRST and accessors rejected', () => {
    let trapExecuted = false;
    const maliciousProxy = new Proxy({ a: 1 }, {
        get(target, prop) {
            trapExecuted = true;
            return target[prop];
        },
        has() {
            trapExecuted = true;
            return true;
        },
        getOwnPropertyDescriptor() {
            trapExecuted = true;
            return { value: 1, enumerable: true, configurable: true };
        }
    });

    // 1. readSafeDataProperty rejects Proxy without executing traps
    assert.throws(() => {
        readSafeDataProperty(maliciousProxy, 'a');
    }, { message: /E_PROXY_FORBIDDEN/ });
    assert.strictEqual(trapExecuted, false, 'Proxy traps must not be executed');

    // 2. validateStructuralBounds rejects Proxy
    assert.throws(() => {
        validateStructuralBounds(maliciousProxy);
    }, { message: /E_PROXY_FORBIDDEN/ });

    // 3. sanitizeClone rejects Proxy
    assert.throws(() => {
        sanitizeClone(maliciousProxy);
    }, { message: /E_PROXY_FORBIDDEN/ });

    // 4. validateAndProjectOutput rejects Proxy
    assert.throws(() => {
        validateAndProjectOutput({ a: 'number' }, maliciousProxy);
    }, { message: /E_PROXY_FORBIDDEN/ });

    // 5. Accessors (getters) are rejected
    let getterExecuted = false;
    const objWithGetter = {
        get sneaky() {
            getterExecuted = true;
            return 999;
        }
    };

    assert.throws(() => {
        readSafeDataProperty(objWithGetter, 'sneaky');
    }, { message: /E_ACCESSOR_FORBIDDEN/ });
    assert.strictEqual(getterExecuted, false, 'Getter must not be executed');

    // 6. Non-finite numbers rejected
    assert.throws(() => validateStructuralBounds({ x: NaN }), /E_NON_FINITE_NUMBER/);
    assert.throws(() => validateStructuralBounds({ x: Infinity }), /E_NON_FINITE_NUMBER/);

    // 7. Circular references rejected
    const circular = { a: 1 };
    circular.self = circular;
    assert.throws(() => validateStructuralBounds(circular), /E_CIRCULAR_REFERENCE/);

    // 8. Max depth exceeded rejected
    let deep = { val: 1 };
    for (let i = 0; i < 40; i++) {
        deep = { child: deep };
    }
    assert.throws(() => validateStructuralBounds(deep), /E_MAX_DEPTH_EXCEEDED/);
});

test('Output Projection - projects strictly onto Object.create(null) and strips prototypes', () => {
    const raw = {
        name: 'Alice',
        score: 100,
        extra: 'ignored'
    };

    const schema = {
        name: 'string',
        score: 'number'
    };

    const projected = validateAndProjectOutput(schema, raw);
    assert.strictEqual(Object.getPrototypeOf(projected), null);
    assert.strictEqual(projected.name, 'Alice');
    assert.strictEqual(projected.score, 100);
    assert.strictEqual(projected.extra, undefined);

    // Schema type mismatch throws
    assert.throws(() => {
        validateAndProjectOutput({ name: 'number' }, { name: 'Alice' });
    }, /E_SCHEMA_TYPE_MISMATCH/);
});

test('Track 2 Outbound Events - validates and projects payload against registered schema', () => {
    const tracker = new HostSurfaceTracker();
    const router = new HostIpcPolicyRouter(tracker);

    const surface = tracker.createSurface('pane-1', {
        secure: true,
        documentId: 'doc-active'
    });

    router.registerSecureEvent('userUpdate', {
        schema: { id: 'number', username: 'string' }
    });

    const out = router.validateTrack2Outbound(surface, 'userUpdate', {
        id: 101,
        username: 'bob',
        unauthorizedField: 'strip-me'
    });

    assert.strictEqual(out.eventName, 'userUpdate');
    assert.strictEqual(out.documentId, 'doc-active');
    assert.strictEqual(out.payload.id, 101);
    assert.strictEqual(out.payload.username, 'bob');
    assert.strictEqual(out.payload.unauthorizedField, undefined);
    assert.strictEqual(Object.getPrototypeOf(out.payload), null);

    // Unregistered event throws
    assert.throws(() => {
        router.validateTrack2Outbound(surface, 'unknownEvent', {});
    }, /E_EVENT_NOT_REGISTERED/);
});

test('Client Bridge Generator - produces hardened bridge script with subframe protection', () => {
    const script = generateSecureBridgeScript({
        surfaceId: 'pane-test',
        documentId: 'doc-test-xyz',
        bootstrapSecret: 'secret-12345',
        enginePort: 9876,
        endpoints: {
            handshake: '/ipc/handshake',
            secure_ws: '/ipc/secure-ws'
        }
    });

    assert.match(script, /window\.parent !== window/);
    assert.match(script, /X-Lotus-Bootstrap/);
    assert.match(script, /secret-12345/);
    assert.match(script, /9876/);
    assert.match(script, /window\.lotus = Object\.freeze/);
});

test('IpcMain.register - dispatches invoke requests and replies with projection or error', async () => {
    const { ipcMain, hostIpcRouter } = require('@lotus-gui/core');

    const surface = hostIpcRouter.surfaceTracker.createSurface('test-win-client', {
        secure: true,
        allowedChannels: ['vault']
    });

    ipcMain.register('vault.testSecret', async () => {
        return { secret: 'secret-xyz', extra: 'drop-me' };
    }, {
        channel: 'vault',
        schema: { secret: 'string' }
    });

    ipcMain.register('forbidden.action', async () => {
        return { done: true };
    }, {
        channel: 'forbidden'
    });

    // 1. Authorized invoke returns projected output
    let receivedReply = null;
    ipcMain.sendTo = (clientId, replyId, data) => {
        receivedReply = { clientId, replyId, data };
    };

    ipcMain.emit('vault.testSecret', { _replyId: 'reply-1', param: 'abc' }, 'test-win-client');
    await new Promise(r => setTimeout(r, 20));

    assert.ok(receivedReply, 'Should receive reply');
    assert.strictEqual(receivedReply.replyId, 'reply-1');
    assert.strictEqual(receivedReply.data.secret, 'secret-xyz');
    assert.strictEqual(receivedReply.data.extra, undefined);
    assert.strictEqual(Object.getPrototypeOf(receivedReply.data), null);

    // 2. Unauthorized invoke returns immediate error without timeout
    let errorReply = null;
    ipcMain.sendTo = (clientId, replyId, data) => {
        errorReply = { clientId, replyId, data };
    };

    ipcMain.emit('forbidden.action', { _replyId: 'reply-2' }, 'test-win-client');
    await new Promise(r => setTimeout(r, 20));

    assert.ok(errorReply, 'Should receive error reply');
    assert.strictEqual(errorReply.replyId, 'reply-2');
    assert.match(errorReply.data._error, /E_CHANNEL_FORBIDDEN/);
});
