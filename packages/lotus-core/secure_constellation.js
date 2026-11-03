//! packages/lotus-core/secure_constellation.js
//! Secure Engine Constellation Manager for Lotus GUI.
//!
//! Manages the shared PID_secure_engine trust domain, monotonic engineInstanceId,
//! reference counting across secure windows/panes, idle teardown cooldown timers,
//! and clean shared-memory teardown across crash recovery.

const EventEmitter = require('node:events');
const { spawnSandboxedEngine } = require('./launcher.js');

class SecureConstellationManager extends EventEmitter {
    constructor(options = {}) {
        super();
        this.supervisorBin = options.supervisorBin;
        this.engineBin = options.engineBin;
        this.idleTimeoutMs = options.idleTimeoutMs ?? 30000; // default 30s

        this._activeInstance = null; // SandboxedEngineProcess
        this._engineInstanceId = 0;
        this._refCount = 0;
        this._surfaces = new Map(); // surfaceId -> surfaceInfo
        this._idleTimer = null;
        this._spawnPromise = null;
    }

    setIdleTimeout(ms) {
        this.idleTimeoutMs = Math.max(0, ms);
    }

    get currentInstanceId() {
        return this._engineInstanceId;
    }

    get isRunning() {
        return this._activeInstance !== null && !this._activeInstance._isTerminated;
    }

    /**
     * Lazily acquires or spawns the shared PID_secure_engine instance.
     * Increments reference count and returns the active instance tagged with monotonic engineInstanceId.
     */
    async acquireSecureEngine(surfaceId, surfaceInfo = {}) {
        if (this._idleTimer) {
            clearTimeout(this._idleTimer);
            this._idleTimer = null;
        }

        if (this._activeInstance && !this._activeInstance._isTerminated) {
            this._refCount++;
            this._surfaces.set(surfaceId, surfaceInfo);
            return {
                instance: this._activeInstance,
                instanceId: this._engineInstanceId,
                enginePid: this._activeInstance.enginePid,
                enginePort: this._activeInstance.enginePort,
            };
        }

        if (this._spawnPromise) {
            const res = await this._spawnPromise;
            this._refCount++;
            this._surfaces.set(surfaceId, surfaceInfo);
            return res;
        }

        this._spawnPromise = (async () => {
            // Monotonic engineInstanceId increment
            this._engineInstanceId++;
            const instanceId = this._engineInstanceId;

            const instance = await spawnSandboxedEngine({
                supervisorBin: this.supervisorBin,
                engineBin: this.engineBin,
            });

            this._activeInstance = instance;

            // Handle supervisor or engine crash
            instance.supervisor.once('exit', (code, sig) => {
                this._handleEngineCrash(instanceId, code, sig);
            });

            return {
                instance,
                instanceId,
                enginePid: instance.enginePid,
                enginePort: instance.enginePort,
            };
        })();

        try {
            const res = await this._spawnPromise;
            this._refCount++;
            this._surfaces.set(surfaceId, surfaceInfo);
            return res;
        } finally {
            this._spawnPromise = null;
        }
    }

    /**
     * Releases a reference to the shared secure engine.
     * When refCount hits 0, starts the idle teardown cooldown timer.
     */
    async releaseSecureEngine(surfaceId) {
        this._surfaces.delete(surfaceId);
        if (this._refCount > 0) {
            this._refCount--;
        }

        if (this._refCount === 0 && this.isRunning) {
            if (this._idleTimer) clearTimeout(this._idleTimer);
            if (this.idleTimeoutMs === 0) {
                await this._teardownEngine();
            } else {
                this._idleTimer = setTimeout(async () => {
                    this._idleTimer = null;
                    if (this._refCount === 0 && this.isRunning) {
                        await this._teardownEngine();
                    }
                }, this.idleTimeoutMs);
                if (typeof this._idleTimer.unref === 'function') {
                    this._idleTimer.unref();
                }
            }
        }
    }

    /**
     * Handles unexpected engine or supervisor crash across the shared failure domain.
     */
    _handleEngineCrash(crashedInstanceId, code, sig) {
        if (this._engineInstanceId !== crashedInstanceId) {
            return; // Ignore stale crash notification from previous incarnation
        }

        if (this._idleTimer) {
            clearTimeout(this._idleTimer);
            this._idleTimer = null;
        }

        const crashedSurfaces = Array.from(this._surfaces.keys());
        this._surfaces.clear();
        this._refCount = 0;
        this._activeInstance = null;

        // Emit crash event for all affected surfaces in the shared failure domain
        this.emit('crashed', {
            instanceId: crashedInstanceId,
            code,
            sig,
            surfaces: crashedSurfaces,
        });
    }

    async _teardownEngine() {
        if (!this._activeInstance) return;
        const inst = this._activeInstance;
        this._activeInstance = null;
        try {
            await inst.terminate();
        } catch {}
    }
}

module.exports = {
    SecureConstellationManager,
};
