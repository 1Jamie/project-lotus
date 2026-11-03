const { test } = require('node:test');
const assert = require('node:assert');
const path = require('node:path');
const fs = require('node:fs');
const { spawn } = require('node:child_process');

const lotusCore = require(path.join(__dirname, '..', '..', 'lotus-core', 'lotus.js'));
const { spawnSandboxedEngine, launcher } = lotusCore;

const supervisorBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-supervisor');
const probeBin = path.resolve(__dirname, '../../lotus-supervisor/target/debug/lotus-sandbox-probe');

test('Sandboxed Launcher - boots supervisor and engine under Stage-A and Stage-B lockdown', async () => {
    if (process.platform !== 'linux') {
        return; // Linux-only test
    }

    assert(fs.existsSync(supervisorBin), `Supervisor binary must exist at ${supervisorBin}`);
    assert(fs.existsSync(probeBin), `Probe binary must exist at ${probeBin}`);

    const engineInstance = await spawnSandboxedEngine({
        supervisorBin,
        engineBin: probeBin,
        timeoutMs: 8000,
    });

    assert(engineInstance, 'Engine instance must be returned');
    assert(engineInstance.enginePid > 0, `Engine PID must be valid: ${engineInstance.enginePid}`);
    assert(engineInstance.enginePort > 0, `Engine network port must be bound: ${engineInstance.enginePort}`);

    // Verify supervisor restricted phase properties
    const supPid = engineInstance.supervisor.pid;
    const supStatus = launcher.parseProcStatus(supPid);
    assert.strictEqual(supStatus.Threads, '1', 'Supervisor must have strictly 1 thread');
    assert.strictEqual(supStatus.NoNewPrivs, '1', 'Supervisor must have NoNewPrivs set');
    assert.strictEqual(supStatus.Seccomp, '2', 'Supervisor must have Seccomp filter active (mode 2)');
    assert.strictEqual(supStatus.CapEff, '0000000000000000', 'Supervisor must have zero effective capabilities');
    assert.strictEqual(supStatus.CapPrm, '0000000000000000', 'Supervisor must have zero permitted capabilities');

    // Verify supervisor descriptor table strictly contains 6 descriptors
    const supFds = launcher.readProcessFds(supPid);
    assert.strictEqual(supFds.length, 6, `Supervisor must have exactly 6 open FDs, found [${supFds.join(', ')}]`);

    // Verify namespace isolation
    const engNs = launcher.readNamespaceInodes(engineInstance.enginePid);
    const supNs = launcher.readNamespaceInodes(supPid);
    assert.notStrictEqual(engNs.pid, supNs.pid, 'Engine PID namespace must differ from supervisor');
    assert.notStrictEqual(engNs.net, supNs.net, 'Engine Net namespace must differ from supervisor');

    // Clean graceful shutdown
    await engineInstance.terminate();
    assert.strictEqual(engineInstance.supervisor.killed || engineInstance.supervisor.exitCode !== null, true);
});

test('Sandboxed Launcher - supervisor single-threaded constructor and linkage audit', () => {
    if (process.platform !== 'linux') return;

    const { execSync } = require('child_process');
    const readelfOutput = execSync(`readelf -d "${supervisorBin}"`, { encoding: 'utf8' });

    // Ensure only libc, libgcc_s, and dynamic linker are linked
    const needed = [...readelfOutput.matchAll(/Shared library: \[(.*?)\]/g)].map(m => m[1]);
    for (const lib of needed) {
        assert(
            lib.startsWith('libc.') || lib.startsWith('libgcc_s.') || lib.startsWith('ld-linux'),
            `Unexpected dynamic library dependency in lotus-supervisor: ${lib}`
        );
    }
});

test('Sandboxed Launcher - supervisor death triggers deterministic child termination via liveness pipe', async () => {
    if (process.platform !== 'linux') return;

    // Launch supervisor with probe
    const child = spawn(supervisorBin, [
        '--supervisor-fd', '3',
        '--control-fd', '4',
        '--engine-bin', probeBin
    ], {
        stdio: ['inherit', 'inherit', 'inherit', 'pipe', 'pipe']
    });

    let enginePid = null;

    await new Promise((resolve, reject) => {
        const timeout = setTimeout(() => reject(new Error('Timed out waiting for STAGE_B_LOCKED')), 5000);

        child.stdio[3].on('data', (d) => {
            try {
                const msg = JSON.parse(d.toString().trim());
                if (msg.type === 'SPAWNED') {
                    enginePid = msg.engine_pid;
                }
            } catch {}
        });

        child.stdio[4].on('data', (d) => {
            const text = d.toString();
            if (text.includes('STAGE_B_LOCKED')) {
                clearTimeout(timeout);
                // Send proceed
                child.stdio[4].write('PROCEED_TO_INIT\n');
                resolve();
            }
        });
    });

    assert(enginePid, 'Engine PID must be set');

    // Kill supervisor forcefully with SIGKILL (simulating unexpected supervisor crash)
    child.kill('SIGKILL');

    // Poll engine PID until it exits (probe monitors LOTUS_LIVENESS_FD and exits on EOF)
    let engineDead = false;
    for (let i = 0; i < 40; i++) {
        await new Promise(r => setTimeout(r, 50));
        if (!fs.existsSync(`/proc/${enginePid}`)) {
            engineDead = true;
            break;
        }
    }

    assert.strictEqual(engineDead, true, 'Engine child must terminate when supervisor dies unexpectedly');
});
