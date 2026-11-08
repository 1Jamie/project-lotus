//! packages/lotus-core/secure_constellation.js
//! Secure Engine Constellation Manager for Lotus GUI.
//!
//! Manages the shared PID_secure_engine trust domain, monotonic engineInstanceId,
//! reference counting across secure windows/panes, idle teardown cooldown timers,
//! and clean shared-memory teardown across crash recovery.

const EventEmitter = require('node:events');
const { spawnSandboxedEngine } = require('./launcher.js');
const hostRegistry = require('./lotus-host-registry.js');

/** Resolve host IPC / native helpers without requiring lotus.js (breaks cycles). */
function _hostBindings() {
    let sendFdsOverSocket = null;
    try {
        sendFdsOverSocket = require('./index.js').sendFdsOverSocket;
    } catch (_) {}
    return {
        sendFdsOverSocket,
        getIpcPort: () => hostRegistry.getIpcPort(),
        hostIpcRouter: hostRegistry.getHostIpcRouter(),
        hostIpcPolicy: hostRegistry.getHostIpcPolicy() || require('./host_ipc_policy.js'),
    };
}

class SecureConstellationManager extends EventEmitter {
    constructor(options = {}) {
        super();
        this.supervisorBin = options.supervisorBin;
        this.engineBin = options.engineBin;
        this.idleTimeoutMs = options.idleTimeoutMs ?? 30000; // default 30s
        this.networkBroker = options.networkBroker || null;
        this.allowLocalhost = options.allowLocalhost ?? false;

        this._activeInstance = null; // SandboxedEngineProcess
        // Global monotonic tag for every engine spawn (shared + isolated).
        this._engineInstanceId = 0;
        // Generation of the *shared* pool only. Isolated spawns must not bump this —
        // otherwise shared LoadStatus/FramePublished events are dropped as "stale"
        // and Track-2 stays provisional forever (E_NAVIGATION_NOT_COMMITTED).
        this._sharedEngineInstanceId = 0;
        this._refCount = 0;
        this._surfaces = new Map(); // surfaceId -> surfaceInfo
        this._isolatedInstances = new Map(); // surfaceId -> { instance, instanceId, surfaceInfo }
        this._idleTimer = null;
        this._spawnPromise = null;

        process.once('exit', () => {
            if (this._activeInstance?.supervisor) {
                try {
                    this._activeInstance.supervisor.kill('SIGKILL');
                } catch {}
            }
            for (const entry of this._isolatedInstances.values()) {
                if (entry?.instance?.supervisor) {
                    try {
                        entry.instance.supervisor.kill('SIGKILL');
                    } catch {}
                }
            }
        });
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

    _sendCreateSurface(instance, surfaceId, surfaceInfo) {
        if (!surfaceInfo?.shm || !instance?.controlFd) return;
        const pixelRwFd = surfaceInfo.shm.pixelRwFd ?? surfaceInfo.shm.rwFd;
        const ackRoFd = surfaceInfo.shm.ackRoFd;
        const lotusCore = _hostBindings();
        if (typeof lotusCore.sendFdsOverSocket === 'function' && typeof pixelRwFd === 'number' && typeof ackRoFd === 'number') {
            const surface = lotusCore.hostIpcRouter?.surfaceTracker?.getSurface(surfaceId);
            let bridgeScript = surfaceInfo.bridgeScript || null;
            if (!bridgeScript && surface && surface.secure) {
                const ipcPort = typeof lotusCore.getIpcPort === 'function' ? lotusCore.getIpcPort() : 0;
                if (ipcPort > 0) {
                    // Bridge bakes documentId + bootstrapSecret. Pin activeDocumentId to the same
                    // value and sync to native *before* the page handshakes, otherwise
                    // /ipc/handshake returns 409 Document ID Mismatch (pending ≠ active).
                    const docId = surface.pendingNavigationId || surface.activeDocumentId;
                    if (docId && surface.activeDocumentId !== docId) {
                        surface.activeDocumentId = docId;
                    }
                    const hip = lotusCore.hostIpcPolicy || require('./host_ipc_policy.js');
                    if (typeof hip.syncSurfacePolicyToNative === 'function') {
                        hip.syncSurfacePolicyToNative(surface);
                    }
                    const secret = surface.bootstrapSecret;
                    // Both handshake HTTP and secure-ws must hit the *host* IPC server
                    // (getIpcPort). instance.enginePort is the jail network-broker bind port —
                    // using it for wsPort makes the bridge connect to the wrong socket (WS 1006)
                    // so heartbeats never land and Pool B watchdog-loops.
                    const trustedOrigins = surface.trustedOrigins && surface.trustedOrigins.size > 0
                        ? Array.from(surface.trustedOrigins)
                        : null;
                    bridgeScript = hip.generateSecureBridgeScript({
                        surfaceId,
                        documentId: docId,
                        bootstrapSecret: secret,
                        enginePort: ipcPort,
                        wsPort: ipcPort,
                        trustedOrigins,
                    });
                    surface._bridgeDocumentId = docId;
                    surface._bridgeSecret = secret;
                }
            }

            const curW = Math.round(surfaceInfo.pane?._layout?.width || surfaceInfo.shm.width || 800);
            const curH = Math.round(surfaceInfo.pane?._layout?.height || surfaceInfo.shm.height || 600);
            const msg = JSON.stringify({
                type: 'CreateSurface',
                surface_id: surfaceId,
                width: curW > 0 ? curW : 800,
                height: curH > 0 ? curH : 600,
                max_width: surfaceInfo.shm.maxWidth || surfaceInfo.shm.max_width || surfaceInfo.shm.width || 800,
                max_height: surfaceInfo.shm.maxHeight || surfaceInfo.shm.max_height || surfaceInfo.shm.height || 600,
                scale_factor: surfaceInfo.pane?.window?._scaleFactor || 1.0,
                url: surfaceInfo.url || 'about:blank',
                bridge_script: bridgeScript,
            }) + '\n';
            try {
                lotusCore.sendFdsOverSocket(instance.controlFd, [pixelRwFd, ackRoFd], msg);
            } catch (e) {
                console.error('[SecureConstellationManager] sendFdsOverSocket error:', e);
            }
        }
    }

    _resolveEngineInstance(surfaceId, pane) {
        if (pane?._engineInstance && !pane._engineInstance._isTerminated) {
            return pane._engineInstance;
        }
        const isolated = this._isolatedInstances.get(surfaceId);
        if (isolated?.instance && !isolated.instance._isTerminated) {
            return isolated.instance;
        }
        if (this._surfaces.has(surfaceId) && this._activeInstance && !this._activeInstance._isTerminated) {
            return this._activeInstance;
        }
        return null;
    }

    /**
     * Per-document secure IPC lifecycle: after commitNavigation rotates documentId
     * (and possibly bootstrapSecret), regenerate the bridge and EvaluateScript it
     * into the live document. CreateSurface only bakes credentials for the first doc.
     */
    _reinjectSecureBridge(surfaceId, surface, pane) {
        if (!surface?.secure || !surface.ipcEligible) return;
        const lotusCore = _hostBindings();
        const ipcPort = typeof lotusCore.getIpcPort === 'function' ? lotusCore.getIpcPort() : 0;
        if (!(ipcPort > 0)) return;

        const instance = this._resolveEngineInstance(surfaceId, pane);
        if (!instance?.controlStream) return;

        const hip = lotusCore.hostIpcPolicy || require('./host_ipc_policy.js');
        if (typeof hip.syncSurfacePolicyToNative === 'function') {
            hip.syncSurfacePolicyToNative(surface);
        }
        const trustedOrigins = surface.trustedOrigins && surface.trustedOrigins.size > 0
            ? Array.from(surface.trustedOrigins)
            : null;
        const bridgeScript = hip.generateSecureBridgeScript({
            surfaceId,
            documentId: surface.activeDocumentId,
            bootstrapSecret: surface.bootstrapSecret,
            enginePort: ipcPort,
            wsPort: ipcPort,
            trustedOrigins,
        });
        surface._bridgeDocumentId = surface.activeDocumentId;
        surface._bridgeSecret = surface.bootstrapSecret;

        try {
            instance.controlStream.write(JSON.stringify({
                type: 'EvaluateScript',
                surface_id: surfaceId,
                script: bridgeScript,
            }) + '\n');
        } catch (e) {
            console.error('[SecureConstellationManager] EvaluateScript (bridge reinject) error:', e);
        }
    }

    _attachControlStream(instance, source = {}) {
        const { surfaceId = null, instanceId = null, isolated = false } = source;
        let engineDataBuffer = '';
        instance.controlStream.on('data', (chunk) => {
            // Drop buffered control traffic from retired engine generations so a
            // dying supervisor cannot failNavigation / commitNavigation the
            // successor's credentials (that desync causes handshake 409s).
            if (instanceId != null) {
                if (isolated && surfaceId) {
                    const entry = this._isolatedInstances.get(surfaceId);
                    if (!entry || entry.instanceId !== instanceId) return;
                } else if (!isolated && this._sharedEngineInstanceId !== instanceId) {
                    return;
                }
            }
            engineDataBuffer += chunk.toString('utf8');
            if (engineDataBuffer.length > 64 * 1024) {
                console.error('[SecureConstellationManager] engineDataBuffer exceeded 64 KiB, terminating');
                instance.terminate().catch(() => {});
                return;
            }
            const lines = engineDataBuffer.split('\n');
            engineDataBuffer = lines.pop();
            for (const line of lines) {
                if (!line.trim()) continue;
                try {
                    const evt = JSON.parse(line.trim());
                    this._handleEngineEvent(evt, { surfaceId, instanceId, isolated });
                } catch {}
            }
        });
    }

    _isCurrentEventSource(evt, source = {}) {
        const instanceId = source.instanceId;
        if (instanceId == null) return true;
        const sid = evt?.surface_id || source.surfaceId;
        if (source.isolated && sid) {
            const entry = this._isolatedInstances.get(sid);
            return Boolean(entry && entry.instanceId === instanceId);
        }
        // Shared pool: compare against shared generation, not the global counter
        // (isolated acquires also increment _engineInstanceId).
        if (!source.isolated) {
            return this._sharedEngineInstanceId === instanceId;
        }
        return true;
    }

    async _retireIsolatedInstance(surfaceId) {
        const prior = this._isolatedInstances.get(surfaceId);
        if (!prior) return null;
        this._isolatedInstances.delete(surfaceId);
        prior.retired = true;
        console.log(`[SecureConstellationManager] Retiring isolated engine instance for surface ${surfaceId} (pid: ${prior.instance?.enginePid || 'unknown'})`);
        if (prior.instance) {
            try {
                prior.instance._isTerminated = true;
            } catch {}
            try {
                if (prior.instance.controlStream && typeof prior.instance.controlStream.removeAllListeners === 'function') {
                    prior.instance.controlStream.removeAllListeners('data');
                }
            } catch {}
            // Force-kill and await kernel process reap (~ms) without waiting for graceful SHUTDOWN.
            try {
                await prior.instance.terminate({ force: true });
            } catch {}
        }
        return prior;
    }

    _spawnIsolatedEngineProcess() {
        const lotusCore = _hostBindings();
        const ipcPort = typeof lotusCore.getIpcPort === 'function' ? lotusCore.getIpcPort() : 0;
        return spawnSandboxedEngine({
            supervisorBin: this.supervisorBin,
            engineBin: this.engineBin,
            passFds: [],
            engineArgs: [],
            networkBroker: this.networkBroker,
            allowLocalhost: this.allowLocalhost,
            ipcPort,
        });
    }

    _ensureProvisionalNavigation(surfaceId, surfaceInfo) {
        const pane = surfaceInfo?.pane;
        const url = surfaceInfo?.url || pane?._pendingUrl || pane?._url || 'about:blank';
        if (!pane) return;
        const lotusCore = _hostBindings();
        const tracker = lotusCore.hostIpcRouter?.surfaceTracker;
        if (!tracker) return;

        let navId = pane._pendingNavId;
        if (!navId || typeof navId !== 'string') {
            navId = `nav-${require('crypto').randomBytes(8).toString('hex')}`;
            pane._pendingNavId = navId;
            pane._pendingUrl = url;
        }

        try {
            const surface = tracker.getSurface(surfaceId);
            if (!surface) return;
            if (surface.pendingNavigationId !== navId) {
                tracker.startNavigation(surfaceId, navId, url);
            }
        } catch (e) {
            console.error('[SecureConstellationManager] _ensureProvisionalNavigation error:', e);
        }
    }

    /**
     * Commit host navigation credentials for a secure surface after LoadStatus complete
     * (also used as a FramePublished heal if complete raced before the pane was mapped).
     */
    _commitSecureNavigation(surfaceId, pane, finalUrl) {
        const lotusCore = _hostBindings();
        const tracker = lotusCore.hostIpcRouter?.surfaceTracker;
        if (!tracker || !pane) return null;
        const surface = tracker.getSurface(surfaceId);
        if (!surface) return null;

        // Already healthy — nothing to do.
        if (
            surface.navigationState === 'committed' &&
            surface.ipcEligible &&
            surface.allowedChannels &&
            surface.allowedChannels.size > 0
        ) {
            return surface;
        }

        const commitUrl = finalUrl
            || pane._pendingUrl
            || pane._url
            || surface.pendingTargetUrl
            || null;
        if (!commitUrl || typeof commitUrl !== 'string') {
            console.warn('[SecureConstellationManager] commit skipped: no URL', surfaceId);
            return null;
        }

        let navId = pane._pendingNavId
            || surface.pendingNavigationId
            || (surface.secure ? surface._bridgeDocumentId : null)
            || surface.activeDocumentId
            || null;
        if (!navId || typeof navId !== 'string') {
            navId = `nav-${require('crypto').randomBytes(8).toString('hex')}`;
        }

        try {
            // Re-arm provisional if needed (failed / cleared pending / heal path).
            if (
                surface.navigationState !== 'provisional' ||
                surface.pendingNavigationId !== navId
            ) {
                tracker.startNavigation(surfaceId, navId, commitUrl);
            }
            // Restore create-time grants if a prior fail-closed wipe emptied them before commit.
            if (
                surface.secure &&
                (!surface.allowedChannels || surface.allowedChannels.size === 0) &&
                Array.isArray(pane._allowedChannels) &&
                pane._allowedChannels.length > 0
            ) {
                surface.allowedChannels = new Set(pane._allowedChannels);
            }
            if (
                surface.secure &&
                (!surface.trustedOrigins || surface.trustedOrigins.size === 0) &&
                Array.isArray(pane._trustedOrigins) &&
                pane._trustedOrigins.length > 0
            ) {
                const { normalizeOriginIpc } = require('./origin_normalize.js');
                surface.trustedOrigins = new Set(pane._trustedOrigins.map((o) => {
                    try { return normalizeOriginIpc(o); } catch (_) { return o; }
                }).filter((o) => o && o !== 'null'));
            }

            const committed = tracker.commitNavigation(surfaceId, navId, commitUrl);
            pane._pendingNavId = null;
            pane._pendingUrl = null;

            // commitNavigation may wipe grants on cross-origin (e.g. null → http). Re-apply
            // the pane's create-time policy so Track-2 invoke is not left channel-empty.
            if (committed?.secure) {
                if (Array.isArray(pane._allowedChannels) && pane._allowedChannels.length > 0) {
                    for (const ch of pane._allowedChannels) committed.allowedChannels.add(ch);
                }
                if (Array.isArray(pane._trustedOrigins) && pane._trustedOrigins.length > 0) {
                    const { normalizeOriginIpc } = require('./origin_normalize.js');
                    for (const o of pane._trustedOrigins) {
                        try {
                            const n = normalizeOriginIpc(o);
                            if (n && n !== 'null') committed.trustedOrigins.add(n);
                        } catch (_) {
                            if (o && o !== 'null') committed.trustedOrigins.add(o);
                        }
                    }
                }
                const hip = lotusCore.hostIpcPolicy || require('./host_ipc_policy.js');
                if (typeof hip.syncSurfacePolicyToNative === 'function') {
                    hip.syncSurfacePolicyToNative(committed);
                }
            }

            if (committed?.secure && committed.ipcEligible) {
                const credsStale =
                    committed._bridgeDocumentId !== committed.activeDocumentId ||
                    committed._bridgeSecret !== committed.bootstrapSecret;
                if (credsStale || !committed._bridgeDocumentId) {
                    this._reinjectSecureBridge(surfaceId, committed, pane);
                }
            }
            return committed;
        } catch (e) {
            console.error('[SecureConstellationManager] commitNavigation error:', e);
            return null;
        }
    }

    /**
     * Pre-warms the shared sandboxed engine process into an ENGINE_READY wait state
     * so it can boot in parallel while the host window and in-process panes initialize.
     */
    prewarm() {
        if (this._idleTimer) {
            clearTimeout(this._idleTimer);
            this._idleTimer = null;
        }
        if (this._activeInstance && !this._activeInstance._isTerminated) {
            return Promise.resolve({
                instance: this._activeInstance,
                instanceId: this._sharedEngineInstanceId,
                enginePid: this._activeInstance.enginePid,
                enginePort: this._activeInstance.enginePort,
                isolated: false,
            });
        }
        if (this._spawnPromise) {
            return this._spawnPromise;
        }

        this._spawnPromise = (async () => {
            this._engineInstanceId++;
            const instanceId = this._engineInstanceId;
            this._sharedEngineInstanceId = instanceId;

            const lotusCore = _hostBindings();
            const ipcPort = typeof lotusCore.getIpcPort === 'function' ? lotusCore.getIpcPort() : 0;

            const instance = await spawnSandboxedEngine({
                supervisorBin: this.supervisorBin,
                engineBin: this.engineBin,
                passFds: [],
                engineArgs: [],
                networkBroker: this.networkBroker,
                allowLocalhost: this.allowLocalhost,
                ipcPort,
            });

            this._activeInstance = instance;
            this._attachControlStream(instance, { instanceId, isolated: false });

            instance.supervisor.once('exit', (code, sig) => {
                this._handleEngineCrash(instanceId, code, sig);
            });

            return {
                instance,
                instanceId,
                enginePid: instance.enginePid,
                enginePort: instance.enginePort,
                isolated: false,
            };
        })();

        this._spawnPromise.finally(() => {
            this._spawnPromise = null;
        }).catch(() => {});

        return this._spawnPromise;
    }

    /**
     * Lazily acquires or spawns the shared PID_secure_engine instance, or spawns a dedicated
     * isolated engine process when `surfaceInfo.isolated === true`.
     * Increments reference count and returns the active instance tagged with monotonic engineInstanceId.
     */
    async acquireSecureEngine(surfaceId, surfaceInfo = {}) {
        // Dedicated isolated sandbox mode (`isolated: true`):
        // Spawns an independent lotus-supervisor + lotus-engine process pair with its own
        // user/PID/net/mount/IPC namespaces, private /tmp sandbox dir, and failure domain.
        if (surfaceInfo && surfaceInfo.isolated === true) {
            // Retire any prior generation first so its exit handler / control-stream
            // cannot delete the successor map entry or mutate surface IPC credentials.
            // Force-kill is non-blocking so spawn of the successor can start immediately.
            await this._retireIsolatedInstance(surfaceId);

            this._engineInstanceId++;
            const instanceId = this._engineInstanceId;

            const instance = await this._spawnIsolatedEngineProcess();

            this._isolatedInstances.set(surfaceId, {
                instance,
                instanceId,
                surfaceInfo,
            });

            this._attachControlStream(instance, { surfaceId, instanceId, isolated: true });

            instance.supervisor.once('exit', (code, sig) => {
                const entry = this._isolatedInstances.get(surfaceId);
                // Generation guard: a late exit from an older supervisor must not
                // delete a newer respawned instance or re-emit crash for it.
                if (!entry || entry.instanceId !== instanceId) return;
                this._isolatedInstances.delete(surfaceId);
                this.emit('crashed', {
                    instanceId,
                    code,
                    sig,
                    surfaces: [surfaceId],
                    isolated: true,
                });
            });

            // Arm / re-sync provisional navigation immediately before CreateSurface so
            // the bridge documentId matches host+native policy. Stale events from the
            // retired generation can clear pending during spawn; this closes that window.
            this._ensureProvisionalNavigation(surfaceId, surfaceInfo);

            this._sendCreateSurface(instance, surfaceId, surfaceInfo);

            return {
                instance,
                instanceId,
                enginePid: instance.enginePid,
                enginePort: instance.enginePort,
                isolated: true,
            };
        }

        if (this._idleTimer) {
            clearTimeout(this._idleTimer);
            this._idleTimer = null;
        }

        if (this._activeInstance && !this._activeInstance._isTerminated) {
            this._refCount++;
            this._surfaces.set(surfaceId, surfaceInfo);
            // Same provisional arm as isolated — keeps bridge documentId ↔ pending nav in sync
            // so LoadStatus complete can commit (otherwise Track-2 stays provisional forever).
            this._ensureProvisionalNavigation(surfaceId, surfaceInfo);
            this._sendCreateSurface(this._activeInstance, surfaceId, surfaceInfo);

            return {
                instance: this._activeInstance,
                instanceId: this._sharedEngineInstanceId,
                enginePid: this._activeInstance.enginePid,
                enginePort: this._activeInstance.enginePort,
                isolated: false,
            };
        }

        if (this._spawnPromise) {
            const res = await this._spawnPromise;
            this._refCount++;
            this._surfaces.set(surfaceId, surfaceInfo);
            this._ensureProvisionalNavigation(surfaceId, surfaceInfo);
            this._sendCreateSurface(this._activeInstance, surfaceId, surfaceInfo);
            return {
                ...res,
                instanceId: this._sharedEngineInstanceId || res.instanceId,
            };
        }

        this._spawnPromise = (async () => {
            // Monotonic engineInstanceId increment
            this._engineInstanceId++;
            const instanceId = this._engineInstanceId;
            this._sharedEngineInstanceId = instanceId;

            const passFds = [];
            const engineArgs = [];

            const lotusCore = _hostBindings();
            const ipcPort = typeof lotusCore.getIpcPort === 'function' ? lotusCore.getIpcPort() : 0;

            const instance = await spawnSandboxedEngine({
                supervisorBin: this.supervisorBin,
                engineBin: this.engineBin,
                passFds,
                engineArgs,
                networkBroker: this.networkBroker,
                allowLocalhost: this.allowLocalhost,
                ipcPort,
            });

            this._activeInstance = instance;

            this._attachControlStream(instance, { instanceId, isolated: false });

            // Handle supervisor or engine crash
            instance.supervisor.once('exit', (code, sig) => {
                this._handleEngineCrash(instanceId, code, sig);
            });

            return {
                instance,
                instanceId,
                enginePid: instance.enginePid,
                enginePort: instance.enginePort,
                isolated: false,
            };
        })();

        try {
            const res = await this._spawnPromise;
            this._refCount++;
            this._surfaces.set(surfaceId, surfaceInfo);
            this._ensureProvisionalNavigation(surfaceId, surfaceInfo);
            this._sendCreateSurface(this._activeInstance, surfaceId, surfaceInfo);
            return res;
        } finally {
            this._spawnPromise = null;
        }
    }

    /**
     * Releases a reference to the shared secure engine (or terminates a dedicated isolated engine).
     * When shared refCount hits 0, starts the idle teardown cooldown timer.
     */
    async releaseSecureEngine(surfaceId) {
        if (this._isolatedInstances.has(surfaceId)) {
            await this._retireIsolatedInstance(surfaceId);
            return;
        }

        this._surfaces.delete(surfaceId);
        if (this._refCount > 0) {
            this._refCount--;
        }

        if (this._activeInstance && this._activeInstance.controlStream && !this._activeInstance.controlStream.destroyed) {
            try {
                this._activeInstance.controlStream.write(JSON.stringify({
                    type: 'DestroySurface',
                    surface_id: surfaceId,
                }) + '\n');
            } catch {}
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
        // Only the shared pool's generation — isolated crashes must not clear shared state,
        // and a later isolated spawn must not make a real shared crash look "stale".
        if (this._sharedEngineInstanceId !== crashedInstanceId) {
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

    _handleEngineEvent(evt, source = {}) {
        if (!evt || !evt.type) return;
        if (!this._isCurrentEventSource(evt, source)) return;

        const surfaceInfo = this._surfaces.get(evt.surface_id)
            || this._isolatedInstances.get(evt.surface_id)?.surfaceInfo;
        const pane = surfaceInfo?.pane;

        if (evt.type === 'LoadStatus') {
            const finalUrl = evt.url;
            const surfaceId = evt.surface_id;
            const lotusCore = _hostBindings();

            if (evt.status === 'complete') {
                if (pane) {
                    this._commitSecureNavigation(surfaceId, pane, finalUrl);
                    pane.emit('load-status', 'complete');
                    if (pane.window) pane.window.emit('load-status', 'complete', pane.id);
                } else {
                    console.warn(
                        '[SecureConstellationManager] LoadStatus complete with no pane mapping',
                        surfaceId
                    );
                }
            } else if (evt.status === 'started') {
                if (pane) {
                    // Keep the navigation id that CreateSurface baked into the bridge.
                    // Replacing it when the engine reports about:blank → real URL desyncs handshake.
                    if (!pane._pendingNavId) {
                        const surface = lotusCore.hostIpcRouter?.surfaceTracker?.getSurface(surfaceId);
                        const targetUrl = finalUrl || pane._pendingUrl || 'about:blank';
                        // Prefer re-arming the already-baked document id over inventing a new one.
                        const navId = (surface?.secure && surface._bridgeDocumentId)
                            ? surface._bridgeDocumentId
                            : `nav-${require('crypto').randomBytes(8).toString('hex')}`;
                        pane._pendingNavId = navId;
                        pane._pendingUrl = targetUrl;
                        try {
                            if (lotusCore.hostIpcRouter?.surfaceTracker) {
                                lotusCore.hostIpcRouter.surfaceTracker.startNavigation(surfaceId, navId, targetUrl);
                            }
                        } catch (e) {
                            console.error('[SecureConstellationManager] startNavigation error:', e);
                        }
                    }
                    pane.emit('load-status', 'started');
                    if (pane.window) pane.window.emit('load-status', 'started', pane.id);
                }
            } else if (evt.status === 'failed') {
                if (pane) {
                    pane._pendingNavId = null;
                    pane._pendingUrl = null;
                    if (lotusCore.hostIpcRouter?.surfaceTracker) {
                        lotusCore.hostIpcRouter.surfaceTracker.failNavigation(surfaceId);
                    }
                    pane.emit('load-status', 'failed');
                    if (pane.window) pane.window.emit('load-status', 'failed', pane.id);
                }
            }
        } else if (evt.type === 'TitleChanged') {
            if (pane) {
                pane.emit('title-changed', evt.title);
            }
        } else if (evt.type === 'FramePublished') {
            if (pane) {
                // Heal Track-2 if LoadStatus complete raced / missed commit (page is painting
                // but host still thinks navigation is provisional → E_NAVIGATION_NOT_COMMITTED).
                const lotusCore = _hostBindings();
                const surface = lotusCore.hostIpcRouter?.surfaceTracker?.getSurface(evt.surface_id);
                if (
                    surface?.secure &&
                    surface.navigationState !== 'committed'
                ) {
                    this._commitSecureNavigation(evt.surface_id, pane, pane._url || pane._pendingUrl);
                }
                pane.emit('frame-published', evt.slot_idx, evt.pub_seq);
                if (pane.window && pane.window.handle && typeof pane.window.handle.requestRedraw === 'function') {
                    pane.window.handle.requestRedraw();
                }
            }
        }
    }

    /**
     * Immediately terminates all active engine and supervisor instances.
     */
    async terminateAll() {
        if (this._idleTimer) {
            clearTimeout(this._idleTimer);
            this._idleTimer = null;
        }
        const promises = [];
        for (const [sid, entry] of this._isolatedInstances.entries()) {
            if (entry && entry.instance) {
                promises.push(entry.instance.terminate({ force: true }).catch(() => {}));
            }
        }
        this._isolatedInstances.clear();
        this._surfaces.clear();
        this._refCount = 0;

        if (this._activeInstance) {
            const inst = this._activeInstance;
            this._activeInstance = null;
            promises.push(inst.terminate({ force: true }).catch(() => {}));
        }

        await Promise.all(promises);
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

