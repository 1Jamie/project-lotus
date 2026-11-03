//! packages/lotus-core/launcher.js
//! Node Host Sandboxed Engine & Supervisor Launcher and Lifecycle Manager.

const { spawn } = require('child_process');
const path = require('path');
const fs = require('fs');

/**
 * Parses /proc/<pid>/status into a key-value dictionary.
 */
function parseProcStatus(pid) {
    try {
        const text = fs.readFileSync(`/proc/${pid}/status`, 'utf8');
        const map = {};
        for (const line of text.split('\n')) {
            const idx = line.indexOf(':');
            if (idx > 0) {
                const key = line.slice(0, idx).trim();
                const val = line.slice(idx + 1).trim();
                map[key] = val;
            }
        }
        return map;
    } catch (e) {
        return null;
    }
}

/**
 * Reads namespace inode numbers for a given pid.
 */
function readNamespaceInodes(pid) {
    const nsDir = `/proc/${pid}/ns`;
    const inodes = {};
    try {
        const entries = fs.readdirSync(nsDir);
        for (const entry of entries) {
            try {
                const target = fs.readlinkSync(path.join(nsDir, entry));
                inodes[entry] = target;
            } catch {}
        }
    } catch {}
    return inodes;
}

/**
 * Reads list of open file descriptors in /proc/<pid>/fd.
 */
function readProcessFds(pid) {
    try {
        return fs.readdirSync(`/proc/${pid}/fd`).map(n => parseInt(n, 10)).sort((a, b) => a - b);
    } catch {
        return [];
    }
}

class SandboxedEngineProcess {
    constructor(supervisorProcess, enginePid, enginePort, supervisorStream, controlStream) {
        this.supervisor = supervisorProcess;
        this.enginePid = enginePid;
        this.enginePort = enginePort;
        this.supervisorStream = supervisorStream;
        this.controlStream = controlStream;
        this._isTerminated = false;
    }

    /**
     * Terminate the engine and supervisor cleanly.
     */
    async terminate() {
        if (this._isTerminated) return;
        this._isTerminated = true;

        return new Promise((resolve) => {
            const timeout = setTimeout(() => {
                try {
                    this.supervisor.kill('SIGKILL');
                } catch {}
                resolve();
            }, 3500);

            this.supervisor.once('exit', () => {
                clearTimeout(timeout);
                resolve();
            });

            try {
                if (this.supervisorStream && !this.supervisorStream.destroyed) {
                    this.supervisorStream.write('SHUTDOWN\n');
                } else {
                    this.supervisor.kill('SIGTERM');
                }
            } catch {
                resolve();
            }
        });
    }
}

/**
 * Spawns lotus-supervisor with the engine binary and performs the STAGE_B_LOCKED /proc audit.
 */
async function spawnSandboxedEngine(options = {}) {
    const supervisorBin = options.supervisorBin || path.resolve(__dirname, '../lotus-supervisor/target/debug/lotus-supervisor');
    const engineBin = options.engineBin || path.resolve(__dirname, '../lotus-supervisor/target/debug/lotus-sandbox-probe');
    const engineArgs = options.engineArgs || [];
    const cacheDir = options.cacheDir;
    const tempDir = options.tempDir;

    if (!fs.existsSync(supervisorBin)) {
        throw new Error(`lotus-supervisor binary not found at ${supervisorBin}`);
    }
    if (!fs.existsSync(engineBin)) {
        throw new Error(`engine binary not found at ${engineBin}`);
    }

    const supArgs = [
        '--supervisor-fd', '3',
        '--control-fd', '4',
        '--engine-bin', engineBin,
    ];
    if (cacheDir) {
        supArgs.push('--cache-dir', cacheDir);
    }
    if (tempDir) {
        supArgs.push('--temp-dir', tempDir);
    }
    if (engineArgs.length > 0) {
        supArgs.push('--', ...engineArgs);
    }

    const supervisorProc = spawn(supervisorBin, supArgs, {
        stdio: ['inherit', 'inherit', 'inherit', 'pipe', 'pipe']
    });

    const supervisorStream = supervisorProc.stdio[3];
    const controlStream = supervisorProc.stdio[4];

    return new Promise((resolve, reject) => {
        let enginePid = null;
        let enginePort = null;
        let stageBLockedReceived = false;
        let startupTimeout = null;

        const cleanupAndReject = (err) => {
            if (startupTimeout) clearTimeout(startupTimeout);
            try {
                supervisorProc.kill('SIGKILL');
            } catch {}
            reject(err);
        };

        startupTimeout = setTimeout(() => {
            cleanupAndReject(new Error('Sandboxed engine startup timed out waiting for STAGE_B_LOCKED/ENGINE_READY'));
        }, options.timeoutMs || 10000);

        supervisorStream.on('data', (data) => {
            const lines = data.toString('utf8').split('\n');
            for (const line of lines) {
                if (!line.trim()) continue;
                try {
                    const msg = JSON.parse(line.trim());
                    if (msg.type === 'SPAWNED') {
                        enginePid = msg.engine_pid;
                        enginePort = msg.engine_port;
                    }
                } catch {}
            }
        });

        let controlBuffer = '';
        controlStream.on('data', (chunk) => {
            controlBuffer += chunk.toString('utf8');
            const lines = controlBuffer.split('\n');
            controlBuffer = lines.pop(); // Keep trailing incomplete line

            for (const line of lines) {
                const trimmed = line.trim();
                if (trimmed === 'STAGE_B_LOCKED') {
                    stageBLockedReceived = true;
                    // =========================================================
                    // AUDIT BOTH SUPERVISOR & ENGINE PROCESSES VIA /proc
                    // =========================================================
                    if (!enginePid) {
                        return cleanupAndReject(new Error('Received STAGE_B_LOCKED before SPAWNED message with engine PID'));
                    }

                    // 1. Audit Engine Process Status
                    const engStatus = parseProcStatus(enginePid);
                    if (!engStatus) {
                        return cleanupAndReject(new Error(`Failed to read /proc/${enginePid}/status`));
                    }
                    if (engStatus.Threads !== '1') {
                        return cleanupAndReject(new Error(`Engine thread invariant failed: expected 1 thread, found ${engStatus.Threads}`));
                    }
                    if (engStatus.NoNewPrivs !== '1') {
                        return cleanupAndReject(new Error(`Engine NoNewPrivs violation: ${engStatus.NoNewPrivs}`));
                    }
                    if (engStatus.Seccomp !== '2') {
                        return cleanupAndReject(new Error(`Engine Seccomp violation: mode ${engStatus.Seccomp} (expected 2 - filter)`));
                    }
                    if (engStatus.CapEff !== '0000000000000000' || engStatus.CapPrm !== '0000000000000000') {
                        return cleanupAndReject(new Error(`Engine capabilities violation: CapEff=${engStatus.CapEff}, CapPrm=${engStatus.CapPrm}`));
                    }

                    // 2. Audit Supervisor Process Status (Restricted Supervision Phase)
                    const supStatus = parseProcStatus(supervisorProc.pid);
                    if (!supStatus) {
                        return cleanupAndReject(new Error(`Failed to read /proc/${supervisorProc.pid}/status`));
                    }
                    if (supStatus.Threads !== '1') {
                        return cleanupAndReject(new Error(`Supervisor thread invariant failed: expected 1 thread, found ${supStatus.Threads}`));
                    }
                    if (supStatus.NoNewPrivs !== '1') {
                        return cleanupAndReject(new Error(`Supervisor NoNewPrivs violation: ${supStatus.NoNewPrivs}`));
                    }
                    if (supStatus.Seccomp !== '2') {
                        return cleanupAndReject(new Error(`Supervisor Seccomp violation: mode ${supStatus.Seccomp} (expected 2 - filter)`));
                    }
                    if (supStatus.CapEff !== '0000000000000000' || supStatus.CapPrm !== '0000000000000000') {
                        return cleanupAndReject(new Error(`Supervisor capabilities violation: CapEff=${supStatus.CapEff}, CapPrm=${supStatus.CapPrm}`));
                    }

                    // 3. Audit Supervisor Allowed File Descriptors [0, 1, 2, 3, liveness_write_fd, engine_pidfd]
                    const supFds = readProcessFds(supervisorProc.pid);
                    if (supFds.length !== 6) {
                        return cleanupAndReject(new Error(`Supervisor descriptor leak: expected 6 open descriptors, found ${supFds.length}: [${supFds.join(', ')}]`));
                    }

                    // 4. Audit Isolated Namespaces (Engine PID and netns must differ from Supervisor)
                    const engNs = readNamespaceInodes(enginePid);
                    const supNs = readNamespaceInodes(supervisorProc.pid);
                    if (engNs.pid && supNs.pid && engNs.pid === supNs.pid) {
                        return cleanupAndReject(new Error(`Engine PID namespace not isolated: ${engNs.pid}`));
                    }
                    if (engNs.net && supNs.net && engNs.net === supNs.net) {
                        return cleanupAndReject(new Error(`Engine Network namespace not isolated: ${engNs.net}`));
                    }

                    // All /proc sandbox verification passed! Send PROCEED_TO_INIT
                    controlStream.write('PROCEED_TO_INIT\n');
                } else if (trimmed === 'ENGINE_READY') {
                    if (!stageBLOCKEDPassed(stageBLockedReceived)) {
                        return cleanupAndReject(new Error('ENGINE_READY received before STAGE_B_LOCKED verification passed'));
                    }
                    clearTimeout(startupTimeout);
                    const instance = new SandboxedEngineProcess(
                        supervisorProc,
                        enginePid,
                        enginePort,
                        supervisorStream,
                        controlStream
                    );
                    resolve(instance);
                }
            }
        });

        supervisorProc.on('error', cleanupAndReject);
        supervisorProc.on('exit', (code, sig) => {
            cleanupAndReject(new Error(`Supervisor exited unexpectedly during launch: code=${code}, sig=${sig}`));
        });
    });
}

function stageBLOCKEDPassed(flag) {
    return flag === true;
}

module.exports = {
    spawnSandboxedEngine,
    SandboxedEngineProcess,
    parseProcStatus,
    readNamespaceInodes,
    readProcessFds,
};

