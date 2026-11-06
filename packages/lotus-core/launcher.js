//! packages/lotus-core/launcher.js
//! Node Host Sandboxed Engine & Supervisor Launcher and Lifecycle Manager.

const { spawn } = require('child_process');
const path = require('path');
const fs = require('fs');
const os = require('os');

/**
 * Parses /proc/<pid>/status into a key-value dictionary.
 */
function parseProcStatus(pid, baseDir = '/proc') {
    try {
        const text = fs.readFileSync(path.join(baseDir, String(pid), 'status'), 'utf8');
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
    constructor(supervisorProcess, enginePid, enginePort, supervisorStream, controlStream, controlFd, brokerServer, allocatedTempDir) {
        this.supervisor = supervisorProcess;
        this.enginePid = enginePid;
        this.enginePort = enginePort;
        this.supervisorStream = supervisorStream;
        this.controlStream = controlStream;
        this.controlFd = controlFd || null;
        this.brokerServer = brokerServer || null;
        this.allocatedTempDir = allocatedTempDir || null;
        this._isTerminated = false;
        if (this.supervisorStream && typeof this.supervisorStream.on === 'function') {
            this.supervisorStream.on('error', () => {});
        }
        if (this.controlStream && typeof this.controlStream.on === 'function') {
            this.controlStream.on('error', () => {});
        }
    }

    /**
     * Terminate the engine and supervisor cleanly.
     */
    async terminate() {
        if (this._isTerminated) return;
        this._isTerminated = true;

        try {
            if (this.controlStream && !this.controlStream.destroyed) {
                try {
                    this.controlStream.write('SHUTDOWN\n');
                } catch {}
            }
            if (this.supervisorStream && !this.supervisorStream.destroyed) {
                this.supervisorStream.write('SHUTDOWN\n');
            } else {
                this.supervisor.kill('SIGTERM');
            }
        } catch {}

        if (this.brokerServer) {
            try {
                await this.brokerServer.close();
            } catch {}
        }

        if (this.allocatedTempDir) {
            try {
                fs.rmSync(this.allocatedTempDir, { recursive: true, force: true });
                this.allocatedTempDir = null;
            } catch {}
        }

        return new Promise((resolve) => {
            const timeout = setTimeout(() => {
                try {
                    this.supervisor.kill('SIGKILL');
                } catch {}
                resolve();
            }, 3500);

            this.supervisor.once('exit', () => {
                clearTimeout(timeout);
                if (this.allocatedTempDir) {
                    try {
                        fs.rmSync(this.allocatedTempDir, { recursive: true, force: true });
                        this.allocatedTempDir = null;
                    } catch {}
                }
                resolve();
            });
        });
    }
}

/**
 * Spawns lotus-supervisor with the engine binary and performs the STAGE_B_LOCKED /proc audit.
 */
async function spawnSandboxedEngine(options = {}) {
    const supervisorBin = options.supervisorBin || path.resolve(__dirname, '../lotus-supervisor/target/debug/lotus-supervisor');
    const engineBin = options.engineBin || path.resolve(__dirname, '../lotus-supervisor/target/debug/lotus-sandbox-probe');
    const engineArgs = [...(options.engineArgs || [])];
    const cacheDir = options.cacheDir;
    
    // Allocate a dedicated mode-0o700 temporary directory when options.tempDir is not supplied (I1 fix)
    let allocatedTempDir = null;
    let tempDir = options.tempDir;
    if (!tempDir) {
        allocatedTempDir = fs.mkdtempSync(path.join(os.tmpdir(), 'lotus-sandbox-'));
        tempDir = allocatedTempDir;
    }

    if (!fs.existsSync(supervisorBin)) {
        throw new Error(`lotus-supervisor binary not found at ${supervisorBin}`);
    }
    if (!fs.existsSync(engineBin)) {
        throw new Error(`engine binary not found at ${engineBin}`);
    }

    let brokerServer = null;
    const shouldEnableNetwork = options.enableNetwork !== false && (options.networkBroker || !engineBin.includes('probe'));
    if (shouldEnableNetwork) {
        try {
            const { NetworkBroker } = require('./network_broker.js');
            const broker = options.networkBroker || new NetworkBroker({
                allowLocalhost: options.allowLocalhost ?? false,
                ipcPort: options.ipcPort
            });
            const socketPath = options.brokerSocketPath || path.join(tempDir, `lotus-broker-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}.sock`);
            brokerServer = await broker.startProxyServer({ socketPath, ipcPort: options.ipcPort });
            engineArgs.push('--broker-socket', socketPath);
        } catch (err) {
            console.warn('[launcher] Failed to start NetworkBroker proxy server:', err);
        }
    }

    const passFds = options.passFds || [];
    const supArgs = [
        '--supervisor-fd', '3',
        '--control-fd', '4',
        '--engine-bin', engineBin,
    ];
    if (passFds.length > 0) {
        supArgs.push('--pass-fds', passFds.join(','));
    }
    if (cacheDir) {
        supArgs.push('--cache-dir', cacheDir);
    }
    if (tempDir) {
        supArgs.push('--temp-dir', tempDir);
    }
    if (engineArgs.length > 0) {
        supArgs.push('--', ...engineArgs);
    }

    let controlSocketPair = null;
    let controlSocket = null;
    let childControlFd = 'pipe';
    try {
        const lotusCore = require('./lotus.js');
        if (typeof lotusCore.createControlSocketPair === 'function') {
            controlSocketPair = lotusCore.createControlSocketPair();
            const hostFd = controlSocketPair.hostFd ?? controlSocketPair.host_fd;
            const childFd = controlSocketPair.childFd ?? controlSocketPair.child_fd;
            childControlFd = childFd;
            const net = require('net');
            controlSocket = new net.Socket({ fd: hostFd, readable: true, writable: true });
        }
    } catch {}

    const stdio = ['inherit', 'inherit', 'inherit', 'pipe', childControlFd];
    if (passFds.length > 0) {
        for (const fd of passFds) {
            if (typeof fd === 'number' && fd >= 0) {
                stdio[fd] = fd;
            }
        }
        for (let i = 0; i < stdio.length; i++) {
            if (stdio[i] === undefined) {
                stdio[i] = 'ignore';
            }
        }
    }

    const supervisorEnv = { ...process.env };
    if (tempDir) {
        supervisorEnv.XDG_RUNTIME_DIR = tempDir;
    } else {
        delete supervisorEnv.XDG_RUNTIME_DIR;
    }
    supervisorEnv.GST_REGISTRY_FORK = 'no';
    supervisorEnv.GST_REGISTRY_DISABLE = 'yes';
    supervisorEnv.PULSE_SERVER = '';
    supervisorEnv.PULSE_AUTOSPAWN = '0';

    const supervisorProc = spawn(supervisorBin, supArgs, {
        stdio,
        env: supervisorEnv,
    });

    if (controlSocketPair) {
        const childFd = controlSocketPair.childFd ?? controlSocketPair.child_fd;
        if (typeof childFd === 'number' && childFd >= 0) {
            try { fs.closeSync(childFd); } catch {}
        }
    }

    const supervisorStream = supervisorProc.stdio[3];
    const controlStream = controlSocket || supervisorProc.stdio[4];

    return new Promise((resolve, reject) => {
        let enginePid = null;
        let enginePort = null;
        let stageBLockedReceived = false;
        let startupTimeout = null;
        let verificationReport = null;

        let onDataHandler = null;
        const cleanupAndReject = (err) => {
            if (startupTimeout) clearTimeout(startupTimeout);
            if (onDataHandler && controlStream) {
                controlStream.off('data', onDataHandler);
                onDataHandler = null;
            }
            if (brokerServer) {
                brokerServer.close().catch(() => {});
            }
            if (allocatedTempDir) {
                try { fs.rmSync(allocatedTempDir, { recursive: true, force: true }); } catch {}
            }
            try {
                supervisorProc.kill('SIGKILL');
            } catch {}
            reject(err);
        };

        supervisorProc.once('exit', () => {
            if (brokerServer) {
                brokerServer.close().catch(() => {});
            }
            if (allocatedTempDir) {
                try { fs.rmSync(allocatedTempDir, { recursive: true, force: true }); } catch {}
            }
        });

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
                        tryAudit();
                    }
                } catch {}
            }
        });

        let auditCompleted = false;
        const tryAudit = () => {
            if (!stageBLockedReceived || !enginePid || auditCompleted) return;
            auditCompleted = true;

            // 1. Audit Engine Process Status
            const engStatus = parseProcStatus(enginePid);
            if (!engStatus) {
                return cleanupAndReject(new Error(`Failed to read /proc/${enginePid}/status`));
            }
            // Strict Invariant: Threads must be exactly 1 at STAGE_B_LOCKED for all engines
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

            // Verify all tasks in /proc/<pid>/task have Seccomp == 2
            const tasksDir = `/proc/${enginePid}/task`;
            try {
                const tasks = fs.readdirSync(tasksDir);
                for (const tid of tasks) {
                    const taskStatus = parseProcStatus(tid, tasksDir);
                    if (taskStatus && taskStatus.Seccomp !== '2') {
                        return cleanupAndReject(new Error(`Engine thread ${tid} Seccomp violation: mode ${taskStatus.Seccomp}`));
                    }
                }
            } catch (err) {
                return cleanupAndReject(new Error(`Failed to read engine task status: ${err.message}`));
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
        };

        let controlBuffer = '';
        onDataHandler = (chunk) => {
            if (controlBuffer.length + chunk.length > 65536) {
                return cleanupAndReject(new Error('controlBuffer exceeded maximum allowed size (64 KiB)'));
            }
            controlBuffer += chunk.toString('utf8');
            const lines = controlBuffer.split('\n');
            controlBuffer = lines.pop(); // Keep trailing incomplete line

            for (const line of lines) {
                const trimmed = line.trim();
                if (trimmed === 'STAGE_B_LOCKED') {
                    stageBLockedReceived = true;
                    tryAudit();
                } else if (trimmed.startsWith('VERIFICATION_REPORT:')) {
                    const rawReport = trimmed.slice('VERIFICATION_REPORT:'.length);
                    const reportMap = {};
                    for (const pair of rawReport.split(',')) {
                        const [k, v] = pair.split(':');
                        if (k) reportMap[k] = v === 'true';
                    }
                    verificationReport = reportMap;
                } else if (trimmed === 'ENGINE_READY') {
                    if (!stageBLOCKEDPassed(stageBLockedReceived)) {
                        return cleanupAndReject(new Error('ENGINE_READY received before STAGE_B_LOCKED verification passed'));
                    }

                    // Audit engine tasks post-startup: every thread spawned by Servo must have Seccomp == 2
                    const tasksDir = `/proc/${enginePid}/task`;
                    try {
                        const tasks = fs.readdirSync(tasksDir);
                        for (const tid of tasks) {
                            const taskStatus = parseProcStatus(tid, tasksDir);
                            if (taskStatus && taskStatus.Seccomp !== '2') {
                                return cleanupAndReject(new Error(`Engine thread ${tid} post-startup Seccomp violation: mode ${taskStatus.Seccomp}`));
                            }
                        }
                    } catch (err) {
                        return cleanupAndReject(new Error(`Failed to audit engine tasks post-startup: ${err.message}`));
                    }

                    const readyIdx = lines.indexOf(line);
                    const remainingLines = readyIdx >= 0 ? lines.slice(readyIdx + 1) : [];
                    let unconsumed = remainingLines.length > 0 ? remainingLines.join('\n') + '\n' : '';
                    if (controlBuffer) {
                        unconsumed += controlBuffer;
                        controlBuffer = '';
                    }

                    clearTimeout(startupTimeout);
                    if (onDataHandler && controlStream) {
                        controlStream.off('data', onDataHandler);
                        onDataHandler = null;
                    }

                    if (unconsumed && controlStream && typeof controlStream.unshift === 'function') {
                        controlStream.unshift(Buffer.from(unconsumed, 'utf8'));
                    }

                    const instance = new SandboxedEngineProcess(
                        supervisorProc,
                        enginePid,
                        enginePort,
                        supervisorStream,
                        controlStream,
                        controlSocketPair ? (controlSocketPair.hostFd ?? controlSocketPair.host_fd) : null,
                        brokerServer,
                        allocatedTempDir
                    );
                    instance.verificationReport = verificationReport;
                    resolve(instance);
                }
            }
        };

        controlStream.on('data', onDataHandler);

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

