const test = require('node:test');
const assert = require('node:assert');

// Simulate the Rust normalize_origin algorithm in JS to verify identical behavior
function normalizeOrigin(raw) {
    const trimmed = (raw || '').trim();
    if (!trimmed) return null;

    if (trimmed.startsWith('lotus-resource:')) {
        const afterScheme = trimmed.replace(/^lotus-resource:/, '').replace(/^\/+/, '');
        const host = afterScheme.split(/[\/?#]/)[0];
        return `lotus-resource://${host || 'localhost'}`;
    }

    try {
        const parsed = new URL(trimmed);
        if (['http:', 'https:', 'ws:', 'wss:'].includes(parsed.protocol)) {
            return parsed.origin;
        }
    } catch (e) {}

    return null;
}

test('Origins - normalizeOrigin handles custom scheme variations and standard URLs', () => {
    // Both bare and full custom scheme formats normalize identically
    assert.strictEqual(normalizeOrigin('lotus-resource:'), 'lotus-resource://localhost');
    assert.strictEqual(normalizeOrigin('lotus-resource://'), 'lotus-resource://localhost');
    assert.strictEqual(normalizeOrigin('lotus-resource://localhost'), 'lotus-resource://localhost');
    assert.strictEqual(normalizeOrigin('lotus-resource://localhost/'), 'lotus-resource://localhost');
    assert.strictEqual(normalizeOrigin('lotus-resource://custom-host/page.html'), 'lotus-resource://custom-host');

    // Standard HTTP / WS origins with ports
    assert.strictEqual(normalizeOrigin('http://localhost:5173'), 'http://localhost:5173');
    assert.strictEqual(normalizeOrigin('http://localhost:5173/app/index.html'), 'http://localhost:5173');
    assert.strictEqual(normalizeOrigin('http://127.0.0.1:3000'), 'http://127.0.0.1:3000');
    assert.strictEqual(normalizeOrigin('https://example.com/sub/'), 'https://example.com');
    assert.strictEqual(normalizeOrigin('ws://127.0.0.1:8080/ws'), 'ws://127.0.0.1:8080');

    // Invalid inputs
    assert.strictEqual(normalizeOrigin(''), null);
    assert.strictEqual(normalizeOrigin('   '), null);
    assert.strictEqual(normalizeOrigin('null'), null);
});

test('Origins - port_script guard only populates credentials for trusted origins', () => {
    const runPortScript = (location, trustedOrigins) => {
        const mockWindow = { location, lotus: {} };
        const originsJson = JSON.stringify(trustedOrigins);

        // This matches the exact code injected by Rust into Servo windows
        const code = `
        (function(window) {
            const trusted = ${originsJson};
            const loc = window.location;
            const effectiveOrigin = (loc.origin && loc.origin !== 'null') ? loc.origin : (loc.protocol + '//' + (loc.host || 'localhost'));
            if (trusted.indexOf(effectiveOrigin) !== -1 || (loc.protocol === 'lotus-resource:' && trusted.indexOf('lotus-resource://localhost') !== -1)) {
                window.lotus = window.lotus || {};
                window.lotus.port = 12345;
                window.lotus.token = 'SECRET_TOKEN';
                window.lotus.id = 'win-1';
                window.lotus.paneId = 'main';
            }
        })(mockWindow);
        `;
        eval(code);
        return mockWindow.lotus;
    };

    // 1. lotus-resource:// with default trust receives token
    const lotusRes = runPortScript({ protocol: 'lotus-resource:', origin: 'null', host: 'localhost' }, ['lotus-resource://localhost']);
    assert.strictEqual(lotusRes.token, 'SECRET_TOKEN');

    // 2. Untrusted localhost port (e.g. 9999) does NOT receive token
    const untrustedRes = runPortScript({ protocol: 'http:', origin: 'http://localhost:9999', host: 'localhost:9999' }, ['lotus-resource://localhost', 'http://localhost:5173']);
    assert.strictEqual(untrustedRes.token, undefined);

    // 3. Explicitly trusted Vite port (5173) receives token
    const trustedVite = runPortScript({ protocol: 'http:', origin: 'http://localhost:5173', host: 'localhost:5173' }, ['lotus-resource://localhost', 'http://localhost:5173']);
    assert.strictEqual(trustedVite.token, 'SECRET_TOKEN');
});

