'use strict';

const test = require('node:test');
const assert = require('node:assert');
const path = require('path');

const registryPath = path.resolve(__dirname, '../../lotus-core/lotus-host-registry.js');

test('host registry — lazy getters, options precedence, and reset', () => {
    // Fresh require so we do not depend on lotus.js boot.
    delete require.cache[registryPath];
    const {
        registerHostRegistry,
        resetHostRegistry,
        getHostIpcRouter,
        getIpcPort,
    } = require(registryPath);

    resetHostRegistry();
    assert.strictEqual(getHostIpcRouter(), null);
    assert.strictEqual(getIpcPort(), 0);

    const mockRouter = { track2Methods: new Map([['ping', {}]]) };
    registerHostRegistry({
        hostIpcRouter: mockRouter,
        getIpcPort: () => 4242,
    });
    assert.strictEqual(getHostIpcRouter(), mockRouter);
    assert.strictEqual(getIpcPort(), 4242);

    // Idempotent partial update
    registerHostRegistry({ getIpcPort: () => 9 });
    assert.strictEqual(getHostIpcRouter(), mockRouter);
    assert.strictEqual(getIpcPort(), 9);

    resetHostRegistry();
    assert.strictEqual(getHostIpcRouter(), null);
    assert.strictEqual(getIpcPort(), 0);
});

test('network_broker — ctor ipcPort wins without lotus.js', () => {
    const { NetworkBroker } = require(path.resolve(__dirname, '../../lotus-core/network_broker.js'));
    const broker = new NetworkBroker({ ipcPort: 5555 });
    assert.strictEqual(broker.ipcPort, 5555);
});
