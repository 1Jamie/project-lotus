/**
 * Lotus Track 1 & Track 2 Host-Authoritative Policy Engine (`host_ipc_policy.js`)
 *
 * Implements:
 * - Host-authoritative surface tracking (provisional vs committed navigation, activeDocumentId, origin, ipcEligible)
 * - Track 1 inbound & post-await generationId/documentId validation
 * - Track 2 inbound & post-await generationId/documentId/channel validation
 * - Proxy-safe and accessor-safe validation (util.types.isProxy checked FIRST)
 * - Two-phase declarative output projection onto Object.create(null)
 * - Outbound host event validation (pane.emit, win.send)
 * - Runtime capability mutation (grantChannel, revokeChannel, setTrustedOrigins)
 * - Secure bridge script generator
 */

const util = require('node:util');
const crypto = require('node:crypto');
const { normalizeOrigin } = require('./network_broker.js');
const { deriveOriginForUrl } = require('./origin_derivation.js');

let track1GenerationCounter = 0;
let track2GenerationCounter = 0;
let track2EventCounter = 0;

const MAX_STRUCTURAL_DEPTH = 32;
const MAX_TOTAL_ELEMENTS = 10000;
const MAX_ARRAY_LENGTH = 10000;

/**
 * Reads an own data property safely from an object, explicitly forbidding Proxies and accessors (getters/setters).
 * Checks util.types.isProxy FIRST before any property inspection.
 */
function readSafeDataProperty(obj, key) {
    if (obj === null || obj === undefined) return undefined;
    if (util.types.isProxy(obj)) {
        throw new TypeError('E_PROXY_FORBIDDEN: Proxy instances are forbidden in Lotus IPC');
    }
    const desc = Object.getOwnPropertyDescriptor(obj, key);
    if (!desc) return undefined;
    if (desc.get || desc.set) {
        throw new TypeError(`E_ACCESSOR_FORBIDDEN: Property "${String(key)}" has getter/setter accessors which are forbidden in Lotus IPC`);
    }
    return desc.value;
}

/**
 * Unified single-pass validation and sanitization cloning.
 * - Checks util.types.isProxy FIRST at every step.
 * - Reads property/index descriptors ONCE and checks for accessors (preventing TOCTOU).
 * - Forbids sparse arrays (holes).
 * - Enforces structural depth and total element budget.
 * - Tracks active traversal ancestors in a Set (cleared on return), allowing shared DAG references
 *   while rejecting actual circular references.
 * - Rejects TypedArrays, ArrayBuffers, Maps, Sets, and complex object instances.
 */
function sanitizeCloneAndValidate(value, depth = 0, ancestors = new Set(), budget = { count: 0 }) {
    if (value === null || value === undefined) return value;

    if (util.types.isProxy(value)) {
        throw new TypeError('E_PROXY_FORBIDDEN: Proxy instances are forbidden in Lotus IPC');
    }

    if (depth > MAX_STRUCTURAL_DEPTH) {
        throw new RangeError(`E_MAX_DEPTH_EXCEEDED: Exceeded max structural depth of ${MAX_STRUCTURAL_DEPTH}`);
    }

    budget.count++;
    if (budget.count > MAX_TOTAL_ELEMENTS) {
        throw new RangeError(`E_MAX_ELEMENTS_EXCEEDED: Exceeded maximum element count of ${MAX_TOTAL_ELEMENTS}`);
    }

    const type = typeof value;
    if (type === 'boolean' || type === 'string') return value;
    if (type === 'number') {
        if (!Number.isFinite(value)) {
            throw new TypeError(`E_NON_FINITE_NUMBER: Found non-finite number ${value}`);
        }
        return value;
    }
    if (type === 'function' || type === 'symbol' || type === 'bigint') {
        throw new TypeError(`E_UNSUPPORTED_TYPE: Type "${type}" is forbidden in Lotus IPC`);
    }

    // Explicitly reject TypedArrays, ArrayBuffers, Maps, Sets, and non-plain complex types
    if (ArrayBuffer.isView(value) || value instanceof ArrayBuffer || (typeof SharedArrayBuffer !== 'undefined' && value instanceof SharedArrayBuffer)) {
        throw new TypeError('E_UNSUPPORTED_TYPE: TypedArray and ArrayBuffer instances are forbidden in Lotus IPC');
    }
    if (value instanceof Map || value instanceof Set) {
        throw new TypeError('E_UNSUPPORTED_TYPE: Map and Set collections are forbidden in Lotus IPC');
    }
    if (value instanceof Date || value instanceof RegExp || value instanceof Promise) {
        throw new TypeError('E_UNSUPPORTED_TYPE: Complex object types (Date, RegExp, Promise) are forbidden in Lotus IPC');
    }

    if (type === 'object') {
        if (ancestors.has(value)) {
            throw new TypeError('E_CIRCULAR_REFERENCE: Circular reference detected');
        }

        ancestors.add(value);
        try {
            if (Array.isArray(value)) {
                if (value.length > MAX_ARRAY_LENGTH) {
                    throw new RangeError(`E_ARRAY_TOO_LARGE: Array length ${value.length} exceeds maximum allowed (${MAX_ARRAY_LENGTH})`);
                }
                const out = new Array(value.length);
                for (let i = 0; i < value.length; i++) {
                    const key = String(i);
                    const desc = Object.getOwnPropertyDescriptor(value, key);
                    if (!desc) {
                        throw new TypeError(`E_SPARSE_ARRAY_FORBIDDEN: Sparse arrays are forbidden in Lotus IPC (missing index ${i})`);
                    }
                    if (desc.get || desc.set) {
                        throw new TypeError(`E_ACCESSOR_FORBIDDEN: Array index "${i}" has getter/setter accessors which are forbidden in Lotus IPC`);
                    }
                    out[i] = sanitizeCloneAndValidate(desc.value, depth + 1, ancestors, budget);
                }
                return out;
            }

            // Plain object validation
            const proto = Object.getPrototypeOf(value);
            if (proto !== Object.prototype && proto !== null) {
                throw new TypeError('E_UNSUPPORTED_TYPE: Only plain objects and null-prototype objects are permitted in Lotus IPC');
            }

            const out = Object.create(null);
            const keys = Object.getOwnPropertyNames(value);
            for (const key of keys) {
                const desc = Object.getOwnPropertyDescriptor(value, key);
                if (!desc) continue;
                if (desc.get || desc.set) {
                    throw new TypeError(`E_ACCESSOR_FORBIDDEN: Property "${key}" has getter/setter accessors which are forbidden in Lotus IPC`);
                }
                out[key] = sanitizeCloneAndValidate(desc.value, depth + 1, ancestors, budget);
            }
            return out;
        } finally {
            ancestors.delete(value);
        }
    }

    return value;
}

/**
 * Validates structural bounds, depth, and circular references.
 * Delegates to sanitizeCloneAndValidate.
 */
function validateStructuralBounds(value, depth = 0, maxDepth = 32, seen = new WeakSet()) {
    sanitizeCloneAndValidate(value, depth);
}

/**
 * Deep-clones and sanitizes an object into clean primitives and Object.create(null).
 * Delegates to sanitizeCloneAndValidate.
 */
function sanitizeClone(value, depth = 0, seen = new WeakSet()) {
    return sanitizeCloneAndValidate(value, depth);
}

/**
 * Projects a value against an optional declarative schema onto Object.create(null).
 * Reads each property descriptor exactly once to eliminate TOCTOU race conditions.
 */
function validateAndProjectOutput(schema, value) {
    if (value === null || value === undefined) return value;
    if (util.types.isProxy(value)) {
        throw new TypeError('E_PROXY_FORBIDDEN: Proxy instances are forbidden in Lotus IPC');
    }

    if (!schema || typeof schema !== 'object') {
        return sanitizeCloneAndValidate(value);
    }

    if (typeof value !== 'object' || Array.isArray(value)) {
        throw new TypeError('E_SCHEMA_TYPE_MISMATCH: Expected plain object for schema projection');
    }

    const proto = Object.getPrototypeOf(value);
    if (proto !== Object.prototype && proto !== null) {
        throw new TypeError('E_UNSUPPORTED_TYPE: Schema projection requires plain object or null prototype');
    }

    const out = Object.create(null);
    const schemaKeys = Object.keys(schema);
    const ancestors = new Set([value]);
    const budget = { count: 1 };

    try {
        for (const key of schemaKeys) {
            const expectedType = schema[key];
            const desc = Object.getOwnPropertyDescriptor(value, key);
            if (!desc) continue;
            if (desc.get || desc.set) {
                throw new TypeError(`E_ACCESSOR_FORBIDDEN: Property "${key}" has getter/setter accessors which are forbidden in Lotus IPC`);
            }
            const val = desc.value;
            if (val === undefined) continue;

            if (typeof expectedType === 'string') {
                if (expectedType === 'string' && typeof val !== 'string') {
                    throw new TypeError(`E_SCHEMA_TYPE_MISMATCH: Property "${key}" expected string, got ${typeof val}`);
                }
                if (expectedType === 'number' && (typeof val !== 'number' || !Number.isFinite(val))) {
                    throw new TypeError(`E_SCHEMA_TYPE_MISMATCH: Property "${key}" expected finite number, got ${typeof val}`);
                }
                if (expectedType === 'boolean' && typeof val !== 'boolean') {
                    throw new TypeError(`E_SCHEMA_TYPE_MISMATCH: Property "${key}" expected boolean, got ${typeof val}`);
                }
            }
            out[key] = sanitizeCloneAndValidate(val, 1, ancestors, budget);
        }
    } finally {
        ancestors.delete(value);
    }

    return out;
}

/**
 * Surface state tracking manager.
 */
function syncSurfacePolicyToNative(surface) {
    let allowedMethods = Array.from(surface.allowedMethods || []);
    if (allowedMethods.length === 0) {
        try {
            const lotusCore = require('./lotus.js');
            if (lotusCore.hostIpcRouter && lotusCore.hostIpcRouter.track2Methods) {
                allowedMethods = Array.from(lotusCore.hostIpcRouter.track2Methods.keys());
            }
        } catch (_) {}
    }

    if (typeof globalThis.__lotusCoreRegisterSurfacePolicy === 'function') {
        globalThis.__lotusCoreRegisterSurfacePolicy(
            surface.surfaceId,
            surface.secure,
            surface.bootstrapSecret,
            surface.activeDocumentId,
            Array.from(surface.allowedChannels || []),
            allowedMethods,
            surface.currentOrigin
        );
    } else {
        try {
            const lotusCore = require('./lotus.js');
            const fn = lotusCore.registerSurfacePolicy || lotusCore.register_surface_policy;
            if (typeof fn === 'function') {
                fn(
                    surface.surfaceId,
                    surface.secure,
                    surface.bootstrapSecret,
                    surface.activeDocumentId,
                    Array.from(surface.allowedChannels || []),
                    allowedMethods,
                    surface.currentOrigin
                );
            }
        } catch (_) {}
    }
}

class HostSurfaceTracker {
    constructor() {
        this.surfaces = new Map();
    }

    createSurface(surfaceId, options = {}) {
        const surface = {
            surfaceId,
            secure: options.secure === true,
            navigationState: 'committed',
            activeDocumentId: options.documentId || `doc-${crypto.randomBytes(8).toString('hex')}`,
            currentOrigin: options.origin ? normalizeOrigin(options.origin) : 'null',
            pendingNavigationId: null,
            pendingOrigin: null,
            pendingTargetUrl: null,
            ipcEligible: options.ipcEligible !== false,
            maxPayloadBytes: options.maxPayloadBytes || 4 * 1024 * 1024,
            trustedOrigins: new Set((options.trustedOrigins || []).map(normalizeOrigin)),
            allowedChannels: new Set(options.allowedChannels || []),
            bootstrapSecret: options.bootstrapSecret || crypto.randomBytes(32).toString('hex'),
            _initialCommitPending: options.initialCommitPending !== false && !options.origin
        };
        this.surfaces.set(surfaceId, surface);
        syncSurfacePolicyToNative(surface);
        return surface;
    }

    getSurface(surfaceId) {
        return this.surfaces.get(surfaceId);
    }

    removeSurface(surfaceId) {
        this.surfaces.delete(surfaceId);
    }

    startNavigation(surfaceId, navigationId, targetUrl) {
        if (!navigationId || typeof navigationId !== 'string') {
            const err = new Error(`E_NAVIGATION_INVALID: Invalid or empty navigationId "${navigationId}"`);
            err.code = 'E_NAVIGATION_INVALID';
            throw err;
        }
        const surface = this.surfaces.get(surfaceId);
        if (!surface) return null;
        surface.navigationState = 'provisional';
        surface.pendingNavigationId = navigationId;
        surface.pendingTargetUrl = targetUrl;
        surface.pendingOrigin = normalizeOrigin(targetUrl);
        if (surface._initialCommitPending) {
            surface.activeDocumentId = navigationId;
            syncSurfacePolicyToNative(surface);
        }
        return surface;
    }

    commitNavigation(surfaceId, navigationId, finalUrl) {
        if (!navigationId || typeof navigationId !== 'string') {
            const err = new Error(`E_NAVIGATION_MISMATCH: Expected non-empty navigationId, got "${navigationId}"`);
            err.code = 'E_NAVIGATION_MISMATCH';
            throw err;
        }

        const surface = this.surfaces.get(surfaceId);
        if (!surface) {
            const err = new Error(`E_SURFACE_NOT_FOUND: Surface "${surfaceId}" not found`);
            err.code = 'E_SURFACE_NOT_FOUND';
            throw err;
        }

        if (!surface.pendingNavigationId || surface.pendingNavigationId !== navigationId || !finalUrl) {
            // Fail closed on commit failure: drop eligibility, clear channels, rotate doc id
            surface.navigationState = 'failed';
            surface.pendingNavigationId = null;
            surface.pendingTargetUrl = null;
            surface.pendingOrigin = null;
            surface.ipcEligible = false;
            surface.allowedChannels = new Set();
            surface.trustedOrigins = new Set();
            surface.activeDocumentId = `doc-failed-${crypto.randomBytes(8).toString('hex')}`;
            surface.bootstrapSecret = crypto.randomBytes(32).toString('hex');
            syncSurfacePolicyToNative(surface);

            const err = new Error(`E_NAVIGATION_MISMATCH: Expected pending navigation ${surface.pendingNavigationId}, got ${navigationId}`);
            err.code = 'E_NAVIGATION_MISMATCH';
            throw err;
        }

        // Authoritatively derive origin and IPC eligibility from the final URL
        const originInfo = deriveOriginForUrl(finalUrl);
        const previousOrigin = surface.currentOrigin;

        surface.currentOrigin = originInfo.origin;
        surface.ipcEligible = originInfo.ipcEligible;
        surface.activeDocumentId = surface.pendingNavigationId;
        surface.pendingNavigationId = null;
        surface.pendingTargetUrl = null;
        surface.pendingOrigin = null;
        surface.navigationState = 'committed';

        // Q4: On initial commit that establishes surface origin, preserve initial grants & credentials.
        // On subsequent cross-origin navigation, revoke all granted channels and rotate bootstrap secret.
        if (surface._initialCommitPending) {
            surface._initialCommitPending = false;
        } else {
            surface.bootstrapSecret = crypto.randomBytes(32).toString('hex');
            if (previousOrigin !== surface.currentOrigin) {
                surface.allowedChannels = new Set();
                surface.trustedOrigins = new Set();
            }
        }

        syncSurfacePolicyToNative(surface);
        return surface;
    }

    failNavigation(surfaceId) {
        const surface = this.surfaces.get(surfaceId);
        if (!surface) return;
        surface.navigationState = 'failed';
        surface.pendingNavigationId = null;
        surface.pendingTargetUrl = null;
        surface.pendingOrigin = null;
        surface.ipcEligible = false;
        surface.allowedChannels = new Set();
        surface.trustedOrigins = new Set();
        surface.activeDocumentId = `doc-failed-${crypto.randomBytes(8).toString('hex')}`;
        syncSurfacePolicyToNative(surface);
    }

    grantChannel(surfaceId, channel) {
        const surface = this.surfaces.get(surfaceId);
        if (surface) {
            surface.allowedChannels.add(channel);
            syncSurfacePolicyToNative(surface);
        }
    }

    revokeChannel(surfaceId, channel) {
        const surface = this.surfaces.get(surfaceId);
        if (surface) {
            surface.allowedChannels.delete(channel);
            syncSurfacePolicyToNative(surface);
        }
    }

    setTrustedOrigins(surfaceId, origins) {
        const surface = this.surfaces.get(surfaceId);
        if (surface) {
            surface.trustedOrigins = new Set(origins.map(normalizeOrigin));
        }
    }
}

/**
 * Policy router coordinating Track 1 and Track 2 registrations and lifecycle validations.
 */
class HostIpcPolicyRouter {
    constructor(surfaceTracker = new HostSurfaceTracker()) {
        this.surfaceTracker = surfaceTracker;
        this.track1Handlers = new Map(); // channel -> { channel, handler, generationId, unregistered }
        this.track2Methods = new Map();  // method -> { method, channel, handler, options, generationId, unregistered }
        this.track2Events = new Map();   // eventName -> { eventName, options, generationId, unregistered }
    }

    // --- Track 1 Operations ---

    handleTrack1(channel, handler) {
        if (typeof channel !== 'string' || !channel) {
            throw new TypeError('Track 1 channel must be a non-empty string');
        }
        if (typeof handler !== 'function') {
            throw new TypeError('Track 1 handler must be a function');
        }

        const entry = {
            channel,
            handler,
            generationId: ++track1GenerationCounter,
            unregistered: false
        };
        this.track1Handlers.set(channel, entry);

        return () => {
            entry.unregistered = true;
            if (this.track1Handlers.get(channel) === entry) {
                this.track1Handlers.delete(channel);
            }
        };
    }

    validateTrack1Inbound(surface, frame, wireBytes = 0, isSecureControlChannel = false) {
        if (isSecureControlChannel) {
            const err = new Error('E_WRONG_CHANNEL: Track 1 frames are unconditionally rejected on secure control channel');
            err.code = 'E_WRONG_CHANNEL';
            throw err;
        }
        if (!surface) {
            const err = new Error('E_SURFACE_NOT_FOUND: Unknown surface');
            err.code = 'E_SURFACE_NOT_FOUND';
            throw err;
        }
        if (wireBytes > surface.maxPayloadBytes) {
            const err = new Error(`E_PAYLOAD_TOO_LARGE: Wire size ${wireBytes} exceeds max ${surface.maxPayloadBytes}`);
            err.code = 'E_PAYLOAD_TOO_LARGE';
            throw err;
        }
        if (surface.secure !== false) {
            const err = new Error('E_SURFACE_SECURE_MISMATCH: Track 1 requires secure === false surface');
            err.code = 'E_SURFACE_SECURE_MISMATCH';
            throw err;
        }
        if (surface.navigationState !== 'committed') {
            const err = new Error(`E_NAVIGATION_NOT_COMMITTED: Navigation state is ${surface.navigationState}`);
            err.code = 'E_NAVIGATION_NOT_COMMITTED';
            throw err;
        }
        if (frame.documentId !== surface.activeDocumentId) {
            const err = new Error(`E_DOCUMENT_MISMATCH: Frame document ${frame.documentId} does not match active ${surface.activeDocumentId}`);
            err.code = 'E_DOCUMENT_MISMATCH';
            throw err;
        }
        if (surface.trustedOrigins.size > 0 && !surface.trustedOrigins.has(surface.currentOrigin)) {
            const err = new Error(`E_ORIGIN_NOT_TRUSTED: Origin ${surface.currentOrigin} is not in trusted set`);
            err.code = 'E_ORIGIN_NOT_TRUSTED';
            throw err;
        }

        return true;
    }

    validateTrack1PostAwait(surface, regEntry, requestDocumentId, requestGenerationId) {
        if (!surface || surface.navigationState !== 'committed') {
            return false;
        }
        if (surface.activeDocumentId !== requestDocumentId) {
            return false;
        }
        const currentEntry = this.track1Handlers.get(regEntry.channel);
        if (
            !currentEntry ||
            currentEntry !== regEntry ||
            currentEntry.generationId !== requestGenerationId ||
            currentEntry.unregistered
        ) {
            return false;
        }
        return true;
    }

    validateTrack1Outbound(surface, channel, data) {
        if (!surface) {
            throw new Error('E_SURFACE_NOT_FOUND: Surface does not exist');
        }
        if (surface.secure !== false) {
            throw new Error('E_SURFACE_SECURE_MISMATCH: Track 1 outbound requires secure === false');
        }
        if (surface.navigationState !== 'committed') {
            throw new Error(`E_NAVIGATION_NOT_COMMITTED: Navigation state is ${surface.navigationState}`);
        }
        if (!surface.ipcEligible) {
            throw new Error('E_IPC_INELIGIBLE: Surface is marked ineligible for IPC');
        }

        return {
            channel,
            data,
            documentId: surface.activeDocumentId
        };
    }

    // --- Track 2 Operations ---

    registerSecureMethod(method, handler, options) {
        if (typeof method !== 'string' || !method) {
            throw new TypeError('Track 2 method must be a non-empty string');
        }
        if (typeof handler !== 'function') {
            throw new TypeError('Track 2 handler must be a function');
        }
        if (!options || typeof options !== 'object' || typeof options.channel !== 'string' || !options.channel) {
            throw new TypeError('Track 2 registration requires mandatory options object with options.channel string');
        }

        const entry = {
            method,
            channel: options.channel,
            handler,
            options,
            generationId: ++track2GenerationCounter,
            unregistered: false
        };
        this.track2Methods.set(method, entry);

        return () => {
            entry.unregistered = true;
            if (this.track2Methods.get(method) === entry) {
                this.track2Methods.delete(method);
            }
        };
    }

    unregisterSecureMethod(method) {
        const entry = this.track2Methods.get(method);
        if (entry) {
            entry.unregistered = true;
            this.track2Methods.delete(method);
            return true;
        }
        return false;
    }

    registerSecureEvent(eventName, options) {
        if (typeof eventName !== 'string' || !eventName) {
            throw new TypeError('Track 2 eventName must be a non-empty string');
        }
        if (!options || typeof options !== 'object') {
            throw new TypeError('Track 2 event registration requires mandatory options object');
        }

        const entry = {
            eventName,
            options,
            generationId: ++track2EventCounter,
            unregistered: false
        };
        this.track2Events.set(eventName, entry);

        return () => {
            entry.unregistered = true;
            if (this.track2Events.get(eventName) === entry) {
                this.track2Events.delete(eventName);
            }
        };
    }

    validateTrack2Inbound(surface, envelope, wireBytes = 0) {
        if (!surface) {
            const err = new Error('E_SURFACE_NOT_FOUND: Unknown surface');
            err.code = 'E_SURFACE_NOT_FOUND';
            throw err;
        }
        if (wireBytes > surface.maxPayloadBytes) {
            const err = new Error(`E_PAYLOAD_TOO_LARGE: Wire size ${wireBytes} exceeds max ${surface.maxPayloadBytes}`);
            err.code = 'E_PAYLOAD_TOO_LARGE';
            throw err;
        }
        if (surface.secure !== true) {
            const err = new Error('E_SURFACE_SECURE_MISMATCH: Track 2 requires secure === true surface');
            err.code = 'E_SURFACE_SECURE_MISMATCH';
            throw err;
        }
        if (surface.navigationState !== 'committed') {
            const err = new Error(`E_NAVIGATION_NOT_COMMITTED: Navigation state is ${surface.navigationState}`);
            err.code = 'E_NAVIGATION_NOT_COMMITTED';
            throw err;
        }
        if (!surface.ipcEligible) {
            const err = new Error('E_IPC_INELIGIBLE: Surface is marked ineligible for IPC');
            err.code = 'E_IPC_INELIGIBLE';
            throw err;
        }
        if (envelope.documentId !== surface.activeDocumentId) {
            const err = new Error(`E_DOCUMENT_MISMATCH: Envelope document ${envelope.documentId} does not match active ${surface.activeDocumentId}`);
            err.code = 'E_DOCUMENT_MISMATCH';
            throw err;
        }

        const regEntry = this.track2Methods.get(envelope.method);
        if (!regEntry || regEntry.unregistered) {
            const err = new Error(`E_METHOD_NOT_FOUND: Method ${envelope.method} is not registered`);
            err.code = 'E_METHOD_NOT_FOUND';
            throw err;
        }
        if (!surface.allowedChannels.has(regEntry.channel)) {
            const err = new Error(`E_CHANNEL_FORBIDDEN: Surface does not have access to channel "${regEntry.channel}"`);
            err.code = 'E_CHANNEL_FORBIDDEN';
            throw err;
        }

        return regEntry;
    }

    validateTrack2PostAwait(surface, regEntry, requestDocumentId, requestGenerationId) {
        if (!surface || surface.navigationState !== 'committed') {
            return false;
        }
        if (surface.activeDocumentId !== requestDocumentId) {
            return false;
        }
        const currentEntry = this.track2Methods.get(regEntry.method);
        if (
            !currentEntry ||
            currentEntry !== regEntry ||
            currentEntry.generationId !== requestGenerationId ||
            currentEntry.unregistered
        ) {
            return false;
        }
        if (!surface.allowedChannels.has(regEntry.channel)) {
            return false;
        }
        return true;
    }

    validateTrack2Outbound(surface, eventName, payload) {
        if (!surface) {
            throw new Error('E_SURFACE_NOT_FOUND: Unknown surface');
        }
        if (surface.secure !== true) {
            throw new Error('E_SURFACE_SECURE_MISMATCH: Track 2 outbound requires secure === true');
        }
        if (surface.navigationState !== 'committed') {
            throw new Error(`E_NAVIGATION_NOT_COMMITTED: Navigation state is ${surface.navigationState}`);
        }
        if (!surface.ipcEligible) {
            throw new Error('E_IPC_INELIGIBLE: Surface is marked ineligible for IPC');
        }

        const eventSpec = this.track2Events.get(eventName);
        if (!eventSpec || eventSpec.unregistered) {
            throw new Error(`E_EVENT_NOT_REGISTERED: Outbound event "${eventName}" is not registered`);
        }

        const projected = validateAndProjectOutput(eventSpec.options.schema, payload);

        return {
            eventName,
            payload: projected,
            documentId: surface.activeDocumentId
        };
    }
}

/**
 * Generates client-side bridge script for Track 2 secure surfaces.
 */
function generateSecureBridgeScript({ surfaceId, documentId, bootstrapSecret, enginePort, endpoints = {} }) {
    const handshakePath = endpoints.handshake || '/ipc/handshake';
    const wsPath = endpoints.secure_ws || '/ipc/secure-ws';

    return `(() => {
    'use strict';
    if (window.parent !== window || window.top !== window) return; // subframes denied bridge

    const TopObject = window.Object;
    const TopFunction = window.Function;
    const TopArray = window.Array;
    const TopError = window.Error;

    function throwSecurityError() {
        const err = new TopError('SecurityError: Lotus IPC is restricted exclusively to top-level browsing contexts');
        err.name = 'DOMException';
        err.code = 18;
        throw err;
    }

    function verifyTopRealm(payload, callback) {
        if (window.parent !== window || window.top !== window) {
            throwSecurityError();
        }
        if (window.frames && window.frames.length > 0) {
            const stack = (new TopError()).stack || '';
            for (let i = 0; i < window.frames.length; i++) {
                try {
                    const subWin = window.frames[i];
                    if (subWin) {
                        if (subWin.location && subWin.location.href && stack.includes(subWin.location.href)) {
                            throwSecurityError();
                        }
                        if (payload && subWin.Object && payload instanceof subWin.Object) {
                            throwSecurityError();
                        }
                        if (callback && subWin.Function && callback instanceof subWin.Function) {
                            throwSecurityError();
                        }
                    }
                } catch (e) {
                    throwSecurityError();
                }
            }
        }
        if (callback !== undefined && callback !== null) {
            if (typeof callback !== 'function' || !(callback instanceof TopFunction) || Object.getPrototypeOf(callback) !== TopFunction.prototype) {
                throwSecurityError();
            }
        }
        if (arguments.length > 0 && callback === undefined) {
            if (payload === undefined || payload === null || typeof payload !== 'object') {
                throwSecurityError();
            }
            const proto = Object.getPrototypeOf(payload);
            if (proto !== TopObject.prototype && proto !== TopArray.prototype) {
                throwSecurityError();
            }
        }
    }

    const surfaceId = ${JSON.stringify(surfaceId)};
    const documentId = ${JSON.stringify(documentId)};
    const bootstrapSecret = ${JSON.stringify(bootstrapSecret)};
    const port = ${Number(enginePort)};
    const handshakeUrl = 'http://127.0.0.1:' + port + ${JSON.stringify(handshakePath)};
    const wsBaseUrl = 'ws://127.0.0.1:' + port + ${JSON.stringify(wsPath)};

    let ws = null;
    let pendingInvocations = new Map();
    let eventListeners = new Map();
    let invocationCounter = 0;

    async function initBridge() {
        try {
            const resp = await fetch(handshakeUrl, {
                method: 'POST',
                headers: {
                    'Content-Type': 'application/json',
                    'X-Lotus-Bootstrap': bootstrapSecret
                },
                body: JSON.stringify({ surfaceId, documentId })
            });
            if (!resp.ok) return;
            const data = await resp.json();
            const ticket = data.ticket;

            const wsUrl = wsBaseUrl + '?surfaceId=' + encodeURIComponent(surfaceId) +
                '&documentId=' + encodeURIComponent(documentId) +
                '&ticket=' + encodeURIComponent(ticket);

            ws = new WebSocket(wsUrl);
            ws.onmessage = (event) => {
                try {
                    const msg = JSON.parse(event.data);
                    if (msg.type === 'reply' && pendingInvocations.has(msg.id)) {
                        const { resolve, reject } = pendingInvocations.get(msg.id);
                        pendingInvocations.delete(msg.id);
                        if (msg.error) reject(new Error(msg.error));
                        else resolve(msg.result);
                    } else if (msg.type === 'event' && eventListeners.has(msg.eventName)) {
                        const cbs = eventListeners.get(msg.eventName);
                        cbs.forEach(cb => cb(msg.payload));
                    }
                } catch (e) {}
            };
        } catch (e) {}
    }

    window.lotus = Object.freeze({
        invoke: function(method, payload) {
            if (this !== window.lotus) throwSecurityError();
            verifyTopRealm();
            verifyTopRealm(payload);
            return new Promise((resolve, reject) => {
                if (!ws || ws.readyState !== WebSocket.OPEN) {
                    return reject(new Error('Lotus IPC is not connected'));
                }
                const id = ++invocationCounter;
                pendingInvocations.set(id, { resolve, reject });
                ws.send(JSON.stringify({
                    type: 'invoke',
                    id,
                    method,
                    payload,
                    documentId
                }));
            });
        },
        on: function(eventName, callback) {
            if (this !== window.lotus) throwSecurityError();
            verifyTopRealm();
            verifyTopRealm(undefined, callback);
            if (!eventListeners.has(eventName)) {
                eventListeners.set(eventName, new Set());
            }
            eventListeners.get(eventName).add(callback);
        },
        off: function(eventName, callback) {
            if (this !== window.lotus) throwSecurityError();
            verifyTopRealm();
            verifyTopRealm(undefined, callback);
            if (eventListeners.has(eventName)) {
                eventListeners.get(eventName).delete(callback);
            }
        }
    });

    initBridge();
})();`;
}

module.exports = {
    readSafeDataProperty,
    validateStructuralBounds,
    sanitizeClone,
    sanitizeCloneAndValidate,
    validateAndProjectOutput,
    HostSurfaceTracker,
    HostIpcPolicyRouter,
    generateSecureBridgeScript
};
