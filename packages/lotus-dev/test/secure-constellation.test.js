const { test } = require('node:test');
const assert = require('node:assert');
const path = require('node:path');

const lotusCore = require(path.join(__dirname, '..', '..', 'lotus-core', 'lotus.js'));
const { SecureConstellationManager } = lotusCore.secureConstellation;

const supervisorBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-supervisor');
const probeBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-sandbox-probe');

test('Secure Constellation - lazy spawning, ref-counted reuse and idle teardown cooldown', async () => {
    if (process.platform !== 'linux') return;

    const constellation = new SecureConstellationManager({
        supervisorBin,
        engineBin: probeBin,
        idleTimeoutMs: 100, // 100ms for fast test
    });

    assert.strictEqual(constellation.isRunning, false, 'Secure engine must not be spawned eagerly');
    assert.strictEqual(constellation.currentInstanceId, 0);

    // 1. Acquire surface 1 -> triggers lazy spawn
    const ref1 = await constellation.acquireSecureEngine('surface-1');
    assert(constellation.isRunning, 'Engine must be running after acquisition');
    assert.strictEqual(ref1.instanceId, 1, 'First instance must have engineInstanceId = 1');
    assert(ref1.enginePid > 0);

    // 2. Acquire surface 2 -> reuses the active secure engine
    const ref2 = await constellation.acquireSecureEngine('surface-2');
    assert.strictEqual(ref2.instanceId, 1, 'Reused surface must share instanceId = 1');
    assert.strictEqual(ref2.enginePid, ref1.enginePid, 'Reused surface must share same PID');

    // 3. Release surface 1 -> engine still running because surface 2 is active
    await constellation.releaseSecureEngine('surface-1');
    assert.strictEqual(constellation.isRunning, true, 'Engine must remain running while refCount > 0');

    // 4. Release surface 2 -> triggers idle teardown timer
    await constellation.releaseSecureEngine('surface-2');

    // Wait for idle cooldown timer (100ms + margin)
    await new Promise(r => setTimeout(r, 250));
    assert.strictEqual(constellation.isRunning, false, 'Engine must teardown after idle timeout expires');

    // 5. Subsequent acquire -> spawns fresh instance with incremented monotonic ID
    const ref3 = await constellation.acquireSecureEngine('surface-3');
    assert.strictEqual(ref3.instanceId, 2, 'Next spawn must have monotonic instanceId = 2');
    assert.strictEqual(constellation.isRunning, true);

    await constellation.releaseSecureEngine('surface-3');
    await new Promise(r => setTimeout(r, 200));
});

test('Secure Constellation - shared failure domain crash reporting and recovery', async () => {
    if (process.platform !== 'linux') return;

    const constellation = new SecureConstellationManager({
        supervisorBin,
        engineBin: probeBin,
        idleTimeoutMs: 5000,
    });

    // Acquire two surfaces
    const ref1 = await constellation.acquireSecureEngine('sec-win-1');
    await constellation.acquireSecureEngine('sec-pane-1');

    assert.strictEqual(constellation.isRunning, true);
    const crashedInstanceId = ref1.instanceId;

    // Listen for crash event
    const crashPromise = new Promise((resolve) => {
        constellation.once('crashed', (event) => {
            resolve(event);
        });
    });

    // Simulate engine crash by killing supervisor forcefully
    ref1.instance.supervisor.kill('SIGKILL');

    const crashEvent = await crashPromise;
    assert.strictEqual(crashEvent.instanceId, crashedInstanceId);
    assert(crashEvent.surfaces.includes('sec-win-1'));
    assert(crashEvent.surfaces.includes('sec-pane-1'));
    assert.strictEqual(constellation.isRunning, false);

    // Recovery: acquire new surface
    const refRecovered = await constellation.acquireSecureEngine('sec-recovered');
    assert.strictEqual(refRecovered.instanceId, crashedInstanceId + 1, 'Must increment instanceId after crash recovery');
    assert.strictEqual(constellation.isRunning, true);

    await constellation.releaseSecureEngine('sec-recovered');
    await new Promise(r => setTimeout(r, 100));
    await constellation._teardownEngine();
});
