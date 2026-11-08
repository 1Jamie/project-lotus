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

test('Audit 4 P1 - Array accessors and sparse arrays are strictly forbidden', () => {
    // 1. Array with getter index
    const evilArr = ['ok'];
    Object.defineProperty(evilArr, 1, {
        get() { return 'evil'; },
        enumerable: true,
        configurable: true
    });

    assert.throws(() => {
        validateAndProjectOutput(null, evilArr);
    }, {
        name: 'TypeError',
        message: /E_ACCESSOR_FORBIDDEN/
    });

    // 2. Sparse array (hole)
    const sparseArr = ['a', , 'c']; // eslint-disable-line no-sparse-arrays
    assert.throws(() => {
        validateAndProjectOutput(null, sparseArr);
    }, {
        name: 'TypeError',
        message: /E_SPARSE_ARRAY_FORBIDDEN/
    });
});

test('Audit 4 P2 - Array length caps, total element budgets, and complex types rejected', () => {
    // 1. Array length cap
    const largeArr = new Array(10001).fill('x');
    assert.throws(() => {
        validateAndProjectOutput(null, largeArr);
    }, {
        name: 'RangeError',
        message: /E_ARRAY_TOO_LARGE/
    });

    // 2. Total element budget across nested structure
    const nested = [];
    for (let i = 0; i < 200; i++) {
        nested.push(new Array(60).fill(1)); // 200 * 60 = 12000 elements
    }
    assert.throws(() => {
        validateAndProjectOutput(null, nested);
    }, {
        name: 'RangeError',
        message: /E_MAX_ELEMENTS_EXCEEDED/
    });

    // 3. TypedArrays, Maps, Sets, Dates, RegExps rejected
    assert.throws(() => {
        validateAndProjectOutput(null, new Uint8Array([1, 2, 3]));
    }, { name: 'TypeError', message: /E_UNSUPPORTED_TYPE/ });

    assert.throws(() => {
        validateAndProjectOutput(null, new Map([['a', 1]]));
    }, { name: 'TypeError', message: /E_UNSUPPORTED_TYPE/ });

    assert.throws(() => {
        validateAndProjectOutput(null, new Set([1, 2]));
    }, { name: 'TypeError', message: /E_UNSUPPORTED_TYPE/ });

    assert.throws(() => {
        validateAndProjectOutput(null, new Date());
    }, { name: 'TypeError', message: /E_UNSUPPORTED_TYPE/ });

    assert.throws(() => {
        validateAndProjectOutput(null, /abc/);
    }, { name: 'TypeError', message: /E_UNSUPPORTED_TYPE/ });
});

test('Audit 4 P3 & P4 - Navigation commit derives authoritative final URL origin and eligibility', () => {
    const tracker = new HostSurfaceTracker();
    const surface = tracker.createSurface('win-nav', {
        secure: true,
        origin: 'https://good.example.com',
        allowedChannels: ['vault', 'telemetry'],
        trustedOrigins: ['https://good.example.com']
    });

    // 1. Cross-origin redirect landing on different origin (P3)
    tracker.startNavigation('win-nav', 'nav-redir-1', 'https://good.example.com/redirect');
    tracker.commitNavigation('win-nav', 'nav-redir-1', 'https://evil.example.com/landed');

    assert.strictEqual(surface.currentOrigin, 'https://evil.example.com');
    // Granted channels and trusted origins MUST be revoked on cross-origin commit
    assert.strictEqual(surface.allowedChannels.size, 0);
    assert.strictEqual(surface.trustedOrigins.size, 0);

    // 2. Navigation committing to data: URL loses IPC eligibility (P4)
    tracker.startNavigation('win-nav', 'nav-data-1', 'data:text/html,<h1>Hello</h1>');
    tracker.commitNavigation('win-nav', 'nav-data-1', 'data:text/html,<h1>Hello</h1>');

    assert.strictEqual(surface.currentOrigin, 'null');
    assert.strictEqual(surface.ipcEligible, false);
});

test('Audit 4 P5 - commitNavigation rejects null or unstarted navigations', () => {
    const tracker = new HostSurfaceTracker();
    tracker.createSurface('win-idle', { secure: true });

    // 1. Commit with no pending navigation throws E_NAVIGATION_MISMATCH
    assert.throws(() => {
        tracker.commitNavigation('win-idle', 'nav-fake', 'https://example.com');
    }, { code: 'E_NAVIGATION_MISMATCH' });

    // 2. Commit with null navigationId throws E_NAVIGATION_MISMATCH
    assert.throws(() => {
        tracker.commitNavigation('win-idle', null, 'https://example.com');
    }, { code: 'E_NAVIGATION_MISMATCH' });
});

test('Audit 4 P6 - Shared references (DAGs) succeed while circular references fail', () => {
    // 1. Shared non-cyclical references succeed
    const shared = { hello: 'world', count: 42 };
    const dag = {
        left: shared,
        right: shared,
        items: [shared, shared]
    };

    const cloned = validateAndProjectOutput(null, dag);
    assert.strictEqual(Object.getPrototypeOf(cloned.left), null);
    assert.strictEqual(Object.getPrototypeOf(cloned.right), null);
    assert.strictEqual(cloned.left.hello, 'world');
    assert.strictEqual(cloned.left.count, 42);
    assert.strictEqual(cloned.right.hello, 'world');
    assert.strictEqual(cloned.right.count, 42);
    assert.strictEqual(cloned.items.length, 2);
    assert.strictEqual(cloned.items[0].hello, 'world');
    assert.strictEqual(cloned.items[1].hello, 'world');

    // 2. True circular reference fails
    const cyclic = { a: 1 };
    cyclic.loop = cyclic;

    assert.throws(() => {
        validateAndProjectOutput(null, cyclic);
    }, {
        name: 'TypeError',
        message: /E_CIRCULAR_REFERENCE/
    });
});

test('Audit 4 S2 - Unregistered surfaces fail closed with E_SURFACE_NOT_FOUND', async () => {
    const { ipcMain } = require('@lotus-gui/core');

    let replyResult = null;
    ipcMain.sendTo = (clientId, replyId, data) => {
        replyResult = { clientId, replyId, data };
    };

    ipcMain.emit('vault.testSecret', { _replyId: 'reply-s2' }, 'unknown-unregistered-surface:main');
    await new Promise(r => setTimeout(r, 20));

    assert.ok(replyResult, 'Should receive reply for unregistered surface');
    assert.strictEqual(replyResult.replyId, 'reply-s2');
    assert.match(replyResult.data._error, /E_SURFACE_NOT_FOUND/);
});

test('Audit 4 Q1 & Q2 - Per-pane surface isolation and fail-closed commit handling', () => {
    const tracker = new HostSurfaceTracker();
    const surfaceA = tracker.createSurface('win-q2:paneA', {
        secure: true,
        allowedChannels: ['vault', 'telemetry'],
        origin: 'https://good.example.com'
    });
    const surfaceB = tracker.createSurface('win-q2:paneB', {
        secure: true,
        allowedChannels: ['vault'],
        origin: 'https://pane-b.example.com'
    });

    // 1. Independent navigations do not clobber each other (Q2)
    tracker.startNavigation('win-q2:paneA', 'nav-a-1', 'https://good.example.com/redir');
    tracker.startNavigation('win-q2:paneB', 'nav-b-1', 'https://pane-b.example.com/page2');

    assert.strictEqual(surfaceA.pendingNavigationId, 'nav-a-1');
    assert.strictEqual(surfaceB.pendingNavigationId, 'nav-b-1');

    // 2. Commit failure on pane A (missing URL or mismatch) fails closed (Q1 & Q2)
    assert.throws(() => {
        tracker.commitNavigation('win-q2:paneA', 'nav-a-1', null);
    }, { code: 'E_NAVIGATION_MISMATCH' });

    assert.strictEqual(surfaceA.ipcEligible, false);
    assert.strictEqual(surfaceA.allowedChannels.size, 0);
    assert.strictEqual(surfaceA.trustedOrigins.size, 0);
    assert.match(surfaceA.activeDocumentId, /^doc-failed-/);

    // 3. Pane B remains unaffected and commits cleanly with its own authority
    tracker.commitNavigation('win-q2:paneB', 'nav-b-1', 'https://pane-b.example.com/page2');
    assert.strictEqual(surfaceB.ipcEligible, true);
    assert.strictEqual(surfaceB.currentOrigin, 'https://pane-b.example.com');
    assert.strictEqual(surfaceB.allowedChannels.has('vault'), true);
});

test('Audit 4 Q4 - Initial navigation preserves grants while cross-origin subsequently drops them', () => {
    const tracker = new HostSurfaceTracker();
    // Surface created without an established origin (typical at window creation)
    const surface = tracker.createSurface('win-q4:main', {
        secure: true,
        allowedChannels: ['vault', 'telemetry'],
        trustedOrigins: ['https://app.example.com']
    });

    // Initial navigation establishes the surface origin without wiping initial grants
    tracker.startNavigation('win-q4:main', 'nav-init', 'https://app.example.com');
    tracker.commitNavigation('win-q4:main', 'nav-init', 'https://app.example.com');

    assert.strictEqual(surface.currentOrigin, 'https://app.example.com');
    assert.strictEqual(surface.allowedChannels.has('vault'), true);
    assert.strictEqual(surface.allowedChannels.has('telemetry'), true);

    // Subsequent cross-origin navigation to untrusted domain revokes all grants
    tracker.startNavigation('win-q4:main', 'nav-subsequent', 'https://evil.example.com');
    tracker.commitNavigation('win-q4:main', 'nav-subsequent', 'https://evil.example.com');

    assert.strictEqual(surface.currentOrigin, 'https://evil.example.com');
    assert.strictEqual(surface.allowedChannels.size, 0);
    assert.strictEqual(surface.trustedOrigins.size, 0);
});

test('Per-document bridge lifecycle - commit rotates documentId; reinject needed when baked creds stale', () => {
    const tracker = new HostSurfaceTracker();
    const surface = tracker.createSurface('win-bridge:main', {
        secure: true,
        documentId: 'nav-first',
        origin: 'https://app.example.com',
        trustedOrigins: ['https://app.example.com'],
        bootstrapSecret: 'secret-initial',
    });

    // Simulate CreateSurface bake
    surface._bridgeDocumentId = 'nav-first';
    surface._bridgeSecret = 'secret-initial';

    tracker.startNavigation('win-bridge:main', 'nav-first', 'https://app.example.com');
    tracker.commitNavigation('win-bridge:main', 'nav-first', 'https://app.example.com');
    assert.strictEqual(surface.activeDocumentId, 'nav-first');
    assert.strictEqual(surface.bootstrapSecret, 'secret-initial');
    // Initial commit keeps baked credentials current — no reinject required
    assert.strictEqual(surface._bridgeDocumentId, surface.activeDocumentId);
    assert.strictEqual(surface._bridgeSecret, surface.bootstrapSecret);

    // Same-origin subsequent navigation: new documentId, secret preserved
    tracker.startNavigation('win-bridge:main', 'nav-second', 'https://app.example.com/page2');
    tracker.commitNavigation('win-bridge:main', 'nav-second', 'https://app.example.com/page2');
    assert.strictEqual(surface.activeDocumentId, 'nav-second');
    assert.strictEqual(surface.bootstrapSecret, 'secret-initial');
    assert.notStrictEqual(surface._bridgeDocumentId, surface.activeDocumentId,
        'baked bridge documentId must be stale after subsequent commit');

    // Cross-origin subsequent navigation: secret rotates as well
    const preCrossSecret = surface.bootstrapSecret;
    surface._bridgeDocumentId = surface.activeDocumentId;
    surface._bridgeSecret = surface.bootstrapSecret;
    tracker.startNavigation('win-bridge:main', 'nav-cross', 'https://evil.example.com');
    tracker.commitNavigation('win-bridge:main', 'nav-cross', 'https://evil.example.com');
    assert.strictEqual(surface.activeDocumentId, 'nav-cross');
    assert.notStrictEqual(surface.bootstrapSecret, preCrossSecret);
    assert.notStrictEqual(surface._bridgeSecret, surface.bootstrapSecret,
        'baked bridge secret must be stale after cross-origin rotation');

    const script = generateSecureBridgeScript({
        surfaceId: 'win-bridge:main',
        documentId: surface.activeDocumentId,
        bootstrapSecret: surface.bootstrapSecret,
        enginePort: 12345,
        wsPort: 12345,
    });
    assert.ok(script.includes('scheduleBridgeRetry'), 'bridge must retry handshake after transient failure');
    assert.ok(script.includes('MAX_BRIDGE_ATTEMPTS'), 'bridge must bound handshake retries');
});
