'use strict';

const test = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const path = require('path');

const {
    normalizeOrigin,
    normalizeOriginIpc,
    normalizeOriginBroker,
    isTrustedOrigin,
    MODE_IPC_TRUST,
    MODE_BROKER_HTTP,
} = require('../../lotus-core/origin_normalize.js');

const goldenPath = path.resolve(__dirname, '../../lotus-core/testdata/origin_golden.json');
const golden = JSON.parse(fs.readFileSync(goldenPath, 'utf8'));

test('Origins — IpcTrust golden vectors (shared with Rust)', () => {
    for (const c of golden.ipc_trust) {
        const got = normalizeOrigin(c.input, MODE_IPC_TRUST);
        assert.strictEqual(
            got,
            c.output,
            `IpcTrust mismatch for ${JSON.stringify(c.input)}: got ${JSON.stringify(got)} want ${JSON.stringify(c.output)}`
        );
        assert.strictEqual(normalizeOriginIpc(c.input), c.output);
    }
});

test('Origins — BrokerHttp golden vectors (shared with Rust)', () => {
    for (const c of golden.broker_http) {
        const got = normalizeOrigin(c.input, MODE_BROKER_HTTP);
        assert.strictEqual(
            got,
            c.output,
            `BrokerHttp mismatch for ${JSON.stringify(c.input)}: got ${JSON.stringify(got)} want ${JSON.stringify(c.output)}`
        );
        assert.strictEqual(normalizeOriginBroker(c.input), c.output);
    }
});

test('Origins — normalized "null" never grants trust', () => {
    const set = new Set(['null', 'https://example.com']);
    assert.strictEqual(isTrustedOrigin('null', set), false);
    assert.strictEqual(isTrustedOrigin('', set), false);
    assert.strictEqual(isTrustedOrigin('https://example.com', set), true);
});

test('Origins — port_script guard only populates credentials for trusted origins', () => {
    const runPortScript = (location, trustedOrigins) => {
        const mockWindow = { location, lotus: {} };
        const originsJson = JSON.stringify(trustedOrigins);

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

    const lotusRes = runPortScript({ protocol: 'lotus-resource:', origin: 'null', host: 'localhost' }, ['lotus-resource://localhost']);
    assert.strictEqual(lotusRes.token, 'SECRET_TOKEN');

    const untrustedRes = runPortScript({ protocol: 'http:', origin: 'http://localhost:9999', host: 'localhost:9999' }, ['lotus-resource://localhost', 'http://localhost:5173']);
    assert.strictEqual(untrustedRes.token, undefined);

    const trustedVite = runPortScript({ protocol: 'http:', origin: 'http://localhost:5173', host: 'localhost:5173' }, ['lotus-resource://localhost', 'http://localhost:5173']);
    assert.strictEqual(trustedVite.token, 'SECRET_TOKEN');
});
