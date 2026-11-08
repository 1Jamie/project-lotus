'use strict';

/**
 * Thin host-side registry so sandbox / policy / broker modules can resolve
 * hostIpcRouter and getIpcPort without require('./lotus.js') cycles.
 *
 * Rules (Phase 2.2):
 * - Never read registry state at module evaluation time — only via getters
 *   inside functions/methods.
 * - Explicit constructor / call options always take precedence over the
 *   registry (standalone unit tests must not need a full lotus.js boot).
 * - Call resetHostRegistry() between tests that install mock routers/ports.
 */

let hostIpcRouter = null;
let hostIpcPolicy = null;
let getIpcPortFn = null;

/**
 * Idempotent partial registration. lotus.js calls this after constructing
 * the real HostIpcPolicyRouter and whenever the IPC listen port is known.
 *
 * @param {{ hostIpcRouter?: object|null, hostIpcPolicy?: object|null, getIpcPort?: (() => number)|null }} partial
 */
function registerHostRegistry(partial = {}) {
    if (Object.prototype.hasOwnProperty.call(partial, 'hostIpcRouter')) {
        hostIpcRouter = partial.hostIpcRouter ?? null;
    }
    if (Object.prototype.hasOwnProperty.call(partial, 'hostIpcPolicy')) {
        hostIpcPolicy = partial.hostIpcPolicy ?? null;
    }
    if (Object.prototype.hasOwnProperty.call(partial, 'getIpcPort')) {
        getIpcPortFn = typeof partial.getIpcPort === 'function' ? partial.getIpcPort : null;
    }
}

/** Clear all registered handles (lotus-dev test isolation). */
function resetHostRegistry() {
    hostIpcRouter = null;
    hostIpcPolicy = null;
    getIpcPortFn = null;
}

function getHostIpcRouter() {
    return hostIpcRouter;
}

function getHostIpcPolicy() {
    return hostIpcPolicy;
}

/**
 * @returns {number} host IPC listen port, or 0 if unset / unavailable
 */
function getIpcPort() {
    if (typeof getIpcPortFn !== 'function') return 0;
    try {
        const p = getIpcPortFn();
        return typeof p === 'number' && p > 0 ? p : 0;
    } catch {
        return 0;
    }
}

module.exports = {
    registerHostRegistry,
    resetHostRegistry,
    getHostIpcRouter,
    getHostIpcPolicy,
    getIpcPort,
};
