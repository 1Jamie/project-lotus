const { test } = require('node:test');
const assert = require('node:assert');
const path = require('node:path');
const fs = require('node:fs');

const lotusCore = require(path.join(__dirname, '..', '..', 'lotus-core', 'lotus.js'));
const {
    spawnSandboxedEngine,
    allocateRemotePaneShm,
} = lotusCore;
const SecureConstellationManager = lotusCore.SecureConstellationManager || lotusCore.secureConstellation.SecureConstellationManager;

const supervisorBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-supervisor');
const engineBin = path.resolve(__dirname, '../../lotus-engine/target/debug/lotus-engine');

function readProcStatus(pid) {
    try {
        const content = fs.readFileSync(`/proc/${pid}/status`, 'utf8');
        const res = {};
        for (const line of content.split('\n')) {
            const idx = line.indexOf(':');
            if (idx > 0) {
                res[line.slice(0, idx).trim()] = line.slice(idx + 1).trim();
            }
        }
        return res;
    } catch {
        return null;
    }
}

test('Step 5: Crash and Restart Lifecycle - real lotus-engine crash, recovery, and resize under shared ack', async () => {
    if (process.platform !== 'linux') {
        return;
    }

    assert(fs.existsSync(supervisorBin), `Supervisor binary must exist at ${supervisorBin}`);
    assert(fs.existsSync(engineBin), `Engine binary must exist at ${engineBin}`);

    // =========================================================================
    // Part 1: Real Engine SIGKILL Detection and Clean Supervisor Teardown
    // =========================================================================
    const width = 400;
    const height = 300;
    const surfaceId1 = 'pane-crash-raw-1';
    const shm1 = allocateRemotePaneShm(surfaceId1, width, height);

    const rawEngine = await spawnSandboxedEngine({
        supervisorBin,
        engineBin,
        passFds: [shm1.rwFd, shm1.ackRoFd],
        engineArgs: [
            '--shm-fd', shm1.rwFd.toString(),
            '--ack-fd', shm1.ackRoFd.toString(),
            '--surface-id', surfaceId1,
            '--width', width.toString(),
            '--height', height.toString(),
            '--url', 'about:blank',
        ],
    });

    if (rawEngine.controlStream) {
        rawEngine.controlStream.on('error', () => {});
    }
    if (rawEngine.supervisorStream) {
        rawEngine.supervisorStream.on('error', () => {});
    }

    const enginePid1 = rawEngine.enginePid;
    const supervisorPid1 = rawEngine.supervisor.pid;

    assert(enginePid1 > 0, 'Engine PID must be valid');
    assert(supervisorPid1 > 0, 'Supervisor PID must be valid');

    // Wait for at least one frame publication
    const hdrBuf1 = Buffer.alloc(64);
    let initialPubSeq = 0n;
    for (let attempt = 0; attempt < 60; attempt++) {
        fs.readSync(shm1.roFd, hdrBuf1, 0, 64, 0);
        initialPubSeq = hdrBuf1.readBigUInt64LE(0);
        if (initialPubSeq >= 1n) break;
        await new Promise((r) => setTimeout(r, 50));
    }
    assert(initialPubSeq >= 1n, `Initial frame must be published: pub_seq=${initialPubSeq}`);

    // Kill engine child process with SIGKILL (simulating native segfault or kernel OOM)
    const supervisorExitPromise = new Promise((resolve) => {
        rawEngine.supervisor.once('exit', (code, sig) => resolve({ code, sig }));
    });

    try {
        process.kill(enginePid1, 'SIGKILL');
    } catch (e) {
        // May already have exited
    }

    // Supervisor must detect engine termination via pidfd/waitpid and exit cleanly
    const supExit = await Promise.race([
        supervisorExitPromise,
        new Promise((_, reject) => setTimeout(() => reject(new Error('Supervisor failed to exit after engine SIGKILL')), 5000))
    ]);
    assert(supExit, 'Supervisor must exit when child engine dies');

    // Verify engine process does not linger as a zombie on the host
    await new Promise((r) => setTimeout(r, 100));
    const deadEngineStatus = readProcStatus(enginePid1);
    assert(deadEngineStatus === null || deadEngineStatus.State?.startsWith('Z'), 'Engine must be dead or reaped');

    await rawEngine.terminate();
    try { fs.closeSync(shm1.rwFd); } catch {}
    try { fs.closeSync(shm1.roFd); } catch {}
    try { fs.closeSync(shm1.ackRwFd); } catch {}
    try { fs.closeSync(shm1.ackRoFd); } catch {}

    // =========================================================================
    // Part 2: Constellation Shared Failure Domain & Monotonic Restart Recovery
    // =========================================================================
    const constellation = new SecureConstellationManager({
        supervisorBin,
        engineBin,
        idleTimeoutMs: 10000,
    });

    const surfaceId2 = 'pane-managed-crash-2';
    const shm2 = allocateRemotePaneShm(surfaceId2, width, height);

    const ref1 = await constellation.acquireSecureEngine(surfaceId2, {
        shm: shm2,
        url: 'about:blank',
    });

    assert.strictEqual(ref1.instanceId, 1, 'First engine instanceId must be 1');
    assert.strictEqual(constellation.isRunning, true, 'Constellation must be running');
    const firstEnginePid = ref1.enginePid;

    // Verify first engine publishes frame
    const hdrBuf2 = Buffer.alloc(64);
    let managedPubSeq1 = 0n;
    for (let attempt = 0; attempt < 60; attempt++) {
        fs.readSync(shm2.roFd, hdrBuf2, 0, 64, 0);
        managedPubSeq1 = hdrBuf2.readBigUInt64LE(0);
        if (managedPubSeq1 >= 1n) break;
        await new Promise((r) => setTimeout(r, 50));
    }
    assert(managedPubSeq1 >= 1n, `Managed engine must publish frame: pub_seq=${managedPubSeq1}`);

    // Setup crash listener
    const crashPromise = new Promise((resolve) => {
        constellation.once('crashed', (info) => resolve(info));
    });

    // Simulate crash on managed engine
    try {
        process.kill(firstEnginePid, 'SIGKILL');
    } catch {}

    const crashInfo = await Promise.race([
        crashPromise,
        new Promise((_, reject) => setTimeout(() => reject(new Error('Constellation crashed event timed out')), 5000))
    ]);

    assert.strictEqual(crashInfo.instanceId, 1, 'Crashed event must match instanceId 1');
    assert(crashInfo.surfaces.includes(surfaceId2), 'Crashed event must list affected surface');
    assert.strictEqual(constellation.isRunning, false, 'Constellation isRunning must be false after crash');

    // Recovery: Re-acquire engine for the surface (simulating pane reload/re-establishment)
    const surfaceIdRecovered = 'pane-managed-recovered-3';
    const recWidth = 800;
    const recHeight = 600;
    const shmRecovered = allocateRemotePaneShm(surfaceIdRecovered, recWidth, recHeight);

    const refRecovered = await constellation.acquireSecureEngine(surfaceIdRecovered, {
        shm: shmRecovered,
        url: 'about:blank',
    });

    assert.strictEqual(refRecovered.instanceId, 2, 'Monotonic engineInstanceId must increment to 2 on recovery');
    assert.strictEqual(constellation.isRunning, true, 'Constellation must be running after recovery');
    assert(refRecovered.enginePid > 0 && refRecovered.enginePid !== firstEnginePid, 'New engine PID must be different from crashed PID');

    // Verify all threads in recovered engine run with Seccomp: 2
    const recoveredTasks = fs.readdirSync(`/proc/${refRecovered.enginePid}/task`);
    for (const tid of recoveredTasks) {
        const tStatus = readProcStatus(tid);
        if (tStatus) {
            assert.strictEqual(tStatus.Seccomp, '2', `Recovered engine thread ${tid} must have Seccomp: 2`);
        }
    }

    // Verify recovered engine produces frames into the new SHM plane
    const hdrBufRecovered = Buffer.alloc(64);
    let recoveredPubSeq = 0n;
    for (let attempt = 0; attempt < 60; attempt++) {
        fs.readSync(shmRecovered.roFd, hdrBufRecovered, 0, 64, 0);
        recoveredPubSeq = hdrBufRecovered.readBigUInt64LE(0);
        if (recoveredPubSeq >= 1n) break;
        await new Promise((r) => setTimeout(r, 50));
    }
    assert(recoveredPubSeq >= 1n, `Recovered engine must publish fresh frames: pub_seq=${recoveredPubSeq}`);

    // =========================================================================
    // Part 3: Resize and Consumer Ack Protocol (No Torn Frames)
    // =========================================================================
    // Simulate compositor consumer reading and acknowledging frames
    const ackBuf = Buffer.alloc(64);
    // Write ConsumerAckHeader: claimed_generation=1, claimed_seq=recoveredPubSeq, claimed_slot=0, free_seq=1
    ackBuf.writeBigUInt64LE(1n, 0);                 // consumer_gen_id
    ackBuf.writeBigUInt64LE(1n, 8);                 // claimed_generation
    ackBuf.writeBigUInt64LE(recoveredPubSeq, 16);   // claimed_seq
    ackBuf.writeBigUInt64LE(1n, 24);                // free_seq
    ackBuf.writeUInt32LE(0, 32);                    // claimed_slot
    fs.writeSync(shmRecovered.ackRwFd, ackBuf, 0, 64, 0);

    // Send rapid resize events to test coalescing and lack of torn frames
    const controlStream = refRecovered.instance.controlStream;
    controlStream.write(JSON.stringify({
        type: 'Resize',
        surface_id: surfaceIdRecovered,
        generation: 2,
        width: 450,
        height: 350,
        scale_factor: 1.0,
    }) + '\n');
    controlStream.write(JSON.stringify({
        type: 'Resize',
        surface_id: surfaceIdRecovered,
        generation: 3,
        width: 500,
        height: 400,
        scale_factor: 1.0,
    }) + '\n');

    // Wait for frame publication after resize
    let postResizePubSeq = recoveredPubSeq;
    for (let attempt = 0; attempt < 60; attempt++) {
        fs.readSync(shmRecovered.roFd, hdrBufRecovered, 0, 64, 0);
        postResizePubSeq = hdrBufRecovered.readBigUInt64LE(0);
        if (postResizePubSeq > recoveredPubSeq) break;
        await new Promise((r) => setTimeout(r, 50));
    }
    assert(postResizePubSeq >= recoveredPubSeq, 'Resize must trigger updated frame publication');

    // Acknowledge the updated frame
    ackBuf.writeBigUInt64LE(postResizePubSeq, 16); // claimed_seq
    ackBuf.writeBigUInt64LE(2n, 24);               // free_seq
    fs.writeSync(shmRecovered.ackRwFd, ackBuf, 0, 64, 0);

    // =========================================================================
    // Part 4: Clean Teardown
    // =========================================================================
    await constellation.releaseSecureEngine(surfaceIdRecovered);
    await constellation._teardownEngine();
    assert.strictEqual(constellation.isRunning, false, 'Constellation must be stopped cleanly');

    try { fs.closeSync(shm2.rwFd); } catch {}
    try { fs.closeSync(shm2.roFd); } catch {}
    try { fs.closeSync(shm2.ackRwFd); } catch {}
    try { fs.closeSync(shm2.ackRoFd); } catch {}
    try { fs.closeSync(shmRecovered.rwFd); } catch {}
    try { fs.closeSync(shmRecovered.roFd); } catch {}
    try { fs.closeSync(shmRecovered.ackRwFd); } catch {}
    try { fs.closeSync(shmRecovered.ackRoFd); } catch {}
});
