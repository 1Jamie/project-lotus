const { test } = require('node:test');
const assert = require('node:assert');
const path = require('node:path');
const fs = require('node:fs');

const lotusCore = require(path.join(__dirname, '..', '..', 'lotus-core', 'lotus.js'));
const { spawnSandboxedEngine, launcher, allocateRemotePaneShm } = lotusCore;

const supervisorBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-supervisor');
const engineBin = path.resolve(__dirname, '../../lotus-engine/target/debug/lotus-engine');

test('Multi-Process Engine Integration - boots real lotus-engine under Stage A & Stage B sandbox', async () => {
    if (process.platform !== 'linux') {
        return; // Linux-only test
    }

    assert(fs.existsSync(supervisorBin), `Supervisor binary must exist at ${supervisorBin}`);
    assert(fs.existsSync(engineBin), `Engine binary must exist at ${engineBin}`);

    // 1. Pre-allocate Shared Memory for the test pane
    const width = 400;
    const height = 300;
    const surfaceId = 'test-win:test-pane-1';
    const shm = allocateRemotePaneShm(surfaceId, width, height);

    assert(shm.rwFd > 0, 'Producer rwFd must be valid');
    assert(shm.roFd > 0, 'Consumer roFd must be valid');
    assert(shm.ackRwFd > 0, 'Consumer ackRwFd must be valid');
    assert(shm.ackRoFd > 0, 'Producer ackRoFd must be valid');
    assert(shm.totalSize > 0, 'SHM totalSize must be positive');

    // 2. Spawn sandboxed engine with real lotus-engine binary
    const engineInstance = await spawnSandboxedEngine({
        supervisorBin,
        engineBin,
        timeoutMs: 15000,
        passFds: [shm.rwFd, shm.ackRoFd],
        engineArgs: [
            '--shm-fd', shm.rwFd.toString(),
            '--ack-fd', shm.ackRoFd.toString(),
            '--surface-id', surfaceId,
            '--width', width.toString(),
            '--height', height.toString(),
            '--url', 'about:blank',
        ],
    });

    assert(engineInstance, 'Engine instance must be returned');
    assert(engineInstance.enginePid > 0, `Engine PID must be valid: ${engineInstance.enginePid}`);

    // 3. Verify Engine Process Status Invariants (/proc/<pid>/status & /proc/<pid>/task/*/status)
    const engStatus = launcher.parseProcStatus(engineInstance.enginePid);
    assert(engStatus, 'Engine /proc status must be accessible');
    assert.strictEqual(engStatus.NoNewPrivs, '1', 'Engine must have NoNewPrivs active');
    assert.strictEqual(engStatus.Seccomp, '2', 'Engine must have Seccomp mode 2 (filter) active');
    assert.strictEqual(engStatus.CapEff, '0000000000000000', 'Engine must have 0 effective capabilities');
    assert.strictEqual(engStatus.CapPrm, '0000000000000000', 'Engine must have 0 permitted capabilities');

    // Audit all threads: every single thread in /proc/<pid>/task must have Seccomp == 2
    const tasksDir = `/proc/${engineInstance.enginePid}/task`;
    const tasks = fs.readdirSync(tasksDir);
    assert(tasks.length > 1, `Engine must have spawned worker threads (found ${tasks.length} threads)`);
    for (const tid of tasks) {
        const taskStatus = launcher.parseProcStatus(tid, tasksDir);
        assert(taskStatus, `Thread ${tid} status must be readable`);
        assert.strictEqual(
            taskStatus.Seccomp,
            '2',
            `Engine thread ${tid} (${taskStatus.Name}) must have Seccomp mode 2 (filter) active`
        );
    }

    // 4. Verify Supervisor Restricted Phase Invariants
    const supPid = engineInstance.supervisor.pid;
    const supStatus = launcher.parseProcStatus(supPid);
    assert(supStatus, 'Supervisor /proc status must be accessible');
    assert.strictEqual(supStatus.Threads, '1', 'Supervisor must have strictly 1 thread');
    assert.strictEqual(supStatus.NoNewPrivs, '1', 'Supervisor must have NoNewPrivs active');
    assert.strictEqual(supStatus.Seccomp, '2', 'Supervisor must have Seccomp mode 2 active');

    // 5. Verify PID and Network Namespace Isolation
    const engNs = launcher.readNamespaceInodes(engineInstance.enginePid);
    const supNs = launcher.readNamespaceInodes(supPid);
    assert.notStrictEqual(engNs.pid, supNs.pid, 'Engine PID namespace must differ from supervisor');
    assert.notStrictEqual(engNs.net, supNs.net, 'Engine Network namespace must differ from supervisor');

    // 6. Test Ping-Pong Protocol over LOTUS_CONTROL_FD
    const pongReceived = await new Promise((resolve, reject) => {
        const timeout = setTimeout(() => reject(new Error('Timed out waiting for Pong from lotus-engine')), 5000);

        const onData = (chunk) => {
            const lines = chunk.toString('utf8').split('\n');
            for (const line of lines) {
                if (!line.trim()) continue;
                try {
                    const msg = JSON.parse(line.trim());
                    if (msg.type === 'Pong') {
                        clearTimeout(timeout);
                        engineInstance.controlStream.removeListener('data', onData);
                        resolve(true);
                    }
                } catch {}
            }
        };

        engineInstance.controlStream.on('data', onData);
        engineInstance.controlStream.write(JSON.stringify({ type: 'Ping' }) + '\n');
    });

    assert.strictEqual(pongReceived, true, 'Engine must reply with Pong over control stream');

    // 7. Verify Shared Memory Publication Invariants on Consumer roFd
    const hdrBuf = Buffer.alloc(64);
    let pubSeq = 0n;
    let activeSlots = 0;
    for (let attempt = 0; attempt < 50; attempt++) {
        fs.readSync(shm.roFd, hdrBuf, 0, 64, 0);
        pubSeq = hdrBuf.readBigUInt64LE(0);
        activeSlots = hdrBuf.readUInt32LE(16);
        if (pubSeq >= 1n && activeSlots === 3) break;
        await new Promise((r) => setTimeout(r, 50));
    }
    assert.strictEqual(activeSlots, 3, 'Active slots in SHM header must be 3');
    assert(pubSeq >= 1n, `pub_seq must be at least 1, got ${pubSeq}`);

    // 8. Clean Graceful Termination
    await engineInstance.terminate();
    assert.strictEqual(engineInstance.supervisor.killed || engineInstance.supervisor.exitCode !== null, true);
});

