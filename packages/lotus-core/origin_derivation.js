//! packages/lotus-core/origin_derivation.js
//! Origin Derivation for Non-Standard Schemes, Calling-Realm Prototype Verification,
//! and Wire Attestation Enforcement.

const { normalizeOrigin } = require('./network_broker.js');

/**
 * Derives the authoritative origin and IPC eligibility for a URL according to the specification:
 * - about:blank and about:srcdoc: Inherit origin & documentId from creator context.
 * - data: URLs: Evaluate strictly to opaque null origin ('null'), unconditionally marked ipcEligible = false.
 * - blob: URLs: Inherit origin from script context that created it.
 * - Web Workers: Do not possess access to window.lotus bridge.
 */
function deriveOriginForUrl(urlStr, creatorContext = {}) {
    if (!urlStr || typeof urlStr !== 'string') {
        return {
            origin: 'null',
            documentId: null,
            ipcEligible: false,
            isOpaque: true,
            scheme: 'unknown',
        };
    }

    const trimmed = urlStr.trim();

    // 1. about:blank and about:srcdoc
    if (trimmed === 'about:blank' || trimmed.startsWith('about:blank?') || trimmed === 'about:srcdoc') {
        const parentOrigin = creatorContext.origin && creatorContext.origin !== 'null' ? creatorContext.origin : 'null';
        const isEligible = Boolean(parentOrigin !== 'null' && creatorContext.ipcEligible !== false);
        return {
            origin: parentOrigin,
            documentId: creatorContext.documentId || null,
            ipcEligible: isEligible,
            isOpaque: parentOrigin === 'null',
            scheme: 'about',
        };
    }

    // 2. data: URLs -> strictly opaque null origin, unconditionally IPC-ineligible
    if (trimmed.startsWith('data:')) {
        return {
            origin: 'null',
            documentId: null,
            ipcEligible: false,
            isOpaque: true,
            scheme: 'data',
        };
    }

    // 3. blob: URLs -> inherits origin from script context
    if (trimmed.startsWith('blob:')) {
        let parsedOrigin = null;
        try {
            const rawUrl = trimmed.slice(5); // strip "blob:"
            const innerUrl = new URL(rawUrl);
            parsedOrigin = normalizeOrigin(innerUrl.origin);
        } catch {}

        const finalOrigin = parsedOrigin || creatorContext.origin || 'null';
        const isEligible = Boolean(finalOrigin !== 'null' && creatorContext.ipcEligible !== false);
        return {
            origin: finalOrigin,
            documentId: creatorContext.documentId || null,
            ipcEligible: isEligible,
            isOpaque: finalOrigin === 'null',
            scheme: 'blob',
        };
    }

    // 4. Standard http:, https:, lotus-resource:
    if (trimmed.startsWith('http://') || trimmed.startsWith('https://')) {
        const norm = normalizeOrigin(trimmed);
        if (!norm || norm === 'null') {
            return {
                origin: 'null',
                documentId: null,
                ipcEligible: false,
                isOpaque: true,
                scheme: 'http',
            };
        }
        return {
            origin: norm,
            documentId: creatorContext.documentId || null,
            ipcEligible: true,
            isOpaque: false,
            scheme: trimmed.startsWith('https://') ? 'https' : 'http',
        };
    }

    if (trimmed.startsWith('lotus-resource:')) {
        return {
            origin: 'lotus-resource://localhost',
            documentId: creatorContext.documentId || null,
            ipcEligible: true,
            isOpaque: false,
            scheme: 'lotus-resource',
        };
    }

    // All other schemes (file:, ftp:, javascript:) evaluate to opaque null, ineligible
    return {
        origin: 'null',
        documentId: null,
        ipcEligible: false,
        isOpaque: true,
        scheme: 'other',
    };
}

/**
 * Validates outbound wire attestation from the native engine against authoritative host policy:
 * - frame.isTopLevel === true
 * - frame.documentId === surface.activeDocumentId
 * - surface.ipcEligible === true
 * - surface.navigationState === 'committed'
 */
function validateOutboundWireAttestation(surface, frame) {
    if (!surface || typeof surface !== 'object') {
        return { valid: false, code: 'E_IPC_SURFACE_NOT_FOUND' };
    }

    if (surface.navigationState !== 'committed') {
        return { valid: false, code: 'E_IPC_NAVIGATION_NOT_COMMITTED' };
    }

    if (surface.ipcEligible !== true) {
        return { valid: false, code: 'E_IPC_ORIGIN_INELIGIBLE' };
    }

    if (!frame || typeof frame !== 'object') {
        return { valid: false, code: 'E_IPC_FRAME_ATTESTATION_MISSING' };
    }

    if (frame.isTopLevel !== true) {
        return { valid: false, code: 'E_IPC_SUBFRAME_RESTRICTED' };
    }

    if (!frame.documentId || frame.documentId !== surface.activeDocumentId) {
        return { valid: false, code: 'E_IPC_DOCUMENT_ID_MISMATCH' };
    }

    return { valid: true };
}

/**
 * Creates prototype calling-realm guard implementation.
 * Ensures caller realm is inspected rather than receiver object, blocking iframe invocations
 * across direct call, .call/.apply/.bind, property getters, and delegated references.
 */
function createPrototypeCallingRealmGuard(options = {}) {
    const documentId = options.documentId || 'doc-default';
    const pipelineId = options.pipelineId || 1000;

    return {
        invoke(callerScope, channel, payload) {
            // Check calling realm: must be top-level browsing context
            if (!callerScope || typeof callerScope !== 'object' || callerScope.isTopLevel !== true) {
                const err = new Error('SecurityError: Lotus IPC is restricted exclusively to top-level browsing contexts');
                err.name = 'DOMException';
                err.code = 18; // DOMException.SECURITY_ERR
                throw err;
            }

            // Attestation wire stamping
            return {
                channel,
                payload,
                frame: {
                    isTopLevel: true,
                    pipelineId,
                    documentId,
                },
            };
        },
    };
}

module.exports = {
    deriveOriginForUrl,
    validateOutboundWireAttestation,
    createPrototypeCallingRealmGuard,
};

