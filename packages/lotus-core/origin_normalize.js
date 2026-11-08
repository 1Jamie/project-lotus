'use strict';

/**
 * Pure-JS origin normalization (BrokerHttp + IpcTrust).
 * Mirrors packages/lotus-core/src/origin.rs and testdata/origin_golden.json.
 * Used by policy/broker code and lotus-dev tests without requiring a native .node.
 */

const MODE_IPC_TRUST = 'ipcTrust';
const MODE_BROKER_HTTP = 'brokerHttp';

function normalizeOrigin(raw, mode = MODE_IPC_TRUST) {
    const trimmed = (raw == null ? '' : String(raw)).trim();
    if (!trimmed) return null;

    if (mode === MODE_BROKER_HTTP) {
        return normalizeBrokerHttp(trimmed);
    }
    return normalizeIpcTrust(trimmed);
}

function normalizeBrokerHttp(trimmed) {
    try {
        const u = new URL(trimmed);
        if (u.protocol !== 'http:' && u.protocol !== 'https:') return null;
        return u.origin; // WHATWG: lowercase host, strip default ports
    } catch {
        return null;
    }
}

function normalizeIpcTrust(trimmed) {
    if (trimmed === 'null') return 'null';

    if (trimmed.startsWith('lotus-resource:')) {
        return normalizeLotusResource(trimmed);
    }

    let u;
    try {
        u = new URL(trimmed);
    } catch {
        return null;
    }

    const protocol = u.protocol.toLowerCase();
    if (protocol === 'http:' || protocol === 'https:' || protocol === 'ws:' || protocol === 'wss:') {
        return serializeSpecialFromUrl(u);
    }
    if (protocol === 'blob:') {
        return normalizeBlob(trimmed);
    }
    // data:, javascript:, about:, file:, and other opaque / non-special → "null"
    return 'null';
}

function normalizeLotusResource(trimmed) {
    const afterScheme = trimmed.replace(/^lotus-resource:/i, '').replace(/^\/+/, '');
    const host = afterScheme.split(/[/?#]/)[0];
    return `lotus-resource://${(host || 'localhost').toLowerCase()}`;
}

function normalizeBlob(blobUrl) {
    const inner = blobUrl.slice('blob:'.length);
    if (inner.startsWith('lotus-resource:')) {
        return 'null';
    }
    try {
        const innerUrl = new URL(inner);
        if (innerUrl.protocol === 'http:' || innerUrl.protocol === 'https:') {
            return serializeSpecialFromUrl(innerUrl);
        }
    } catch {
        // fall through
    }
    return 'null';
}

/**
 * Match WHATWG URL.origin for http(s)/ws(s): lowercase host, strip default ports,
 * preserve IPv6 brackets.
 */
function serializeSpecialFromUrl(u) {
    // URL.origin already does the right thing for http(s)/ws(s) in Node.
    // Force lowercase on the whole origin string for host case (Node already lowercases).
    return u.origin;
}

/**
 * Trust membership: normalized "null" never grants trust (opaque ≠ wildcard).
 */
function isTrustedOrigin(normalized, trustedSet) {
    if (!normalized || normalized === 'null') return false;
    if (trustedSet instanceof Set) return trustedSet.has(normalized);
    if (Array.isArray(trustedSet)) return trustedSet.indexOf(normalized) !== -1;
    return false;
}

module.exports = {
    MODE_IPC_TRUST,
    MODE_BROKER_HTTP,
    normalizeOrigin,
    normalizeOriginIpc: (raw) => normalizeOrigin(raw, MODE_IPC_TRUST),
    normalizeOriginBroker: (raw) => normalizeOrigin(raw, MODE_BROKER_HTTP),
    isTrustedOrigin,
};
