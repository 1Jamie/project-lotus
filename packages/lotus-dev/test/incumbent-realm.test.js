const { test } = require('node:test');
const assert = require('node:assert');
const path = require('node:path');

const lotusCore = require(path.join(__dirname, '..', '..', 'lotus-core', 'lotus.js'));
const {
    deriveOriginForUrl,
    validateOutboundWireAttestation,
    createPrototypeCallingRealmGuard
} = lotusCore.originDerivation;

test('Incumbent Realm Gate - native prototype gate enforces calling realm over receiver object', () => {
    const guard = createPrototypeCallingRealmGuard({
        documentId: 'doc-alpha',
        pipelineId: 42,
    });

    const topLevelScope = {
        name: 'window_top',
        isTopLevel: true,
        origin: 'https://example.com',
    };

    const sameOriginIframeScope = {
        name: 'window_iframe_same',
        isTopLevel: false,
        origin: 'https://example.com',
    };

    const crossOriginIframeScope = {
        name: 'window_iframe_cross',
        isTopLevel: false,
        origin: 'https://evil.example',
    };

    // 1. Top-level document invocation -> Allowed, stamps attestation
    const resTop = guard.invoke(topLevelScope, 'test_method', { value: 1 });
    assert.deepStrictEqual(resTop.frame, {
        isTopLevel: true,
        pipelineId: 42,
        documentId: 'doc-alpha',
    });
    assert.strictEqual(resTop.channel, 'test_method');

    // 2. Same-origin iframe attempting to use parent bridge -> MUST BE BLOCKED
    assert.throws(
        () => {
            guard.invoke(sameOriginIframeScope, 'test_method', { value: 2 });
        },
        (err) => {
            assert.strictEqual(err.name, 'DOMException');
            assert.strictEqual(err.code, 18);
            assert.match(err.message, /restricted exclusively to top-level browsing contexts/);
            return true;
        }
    );

    // 3. Cross-origin iframe attempting to use parent bridge -> MUST BE BLOCKED
    assert.throws(
        () => {
            guard.invoke(crossOriginIframeScope, 'test_method', { value: 3 });
        },
        (err) => {
            assert.strictEqual(err.name, 'DOMException');
            assert.strictEqual(err.code, 18);
            assert.match(err.message, /restricted exclusively to top-level browsing contexts/);
            return true;
        }
    );

    // 4. Delegated reference / .call / .apply where receiver object is spoofed as parent
    const delegatedInvoke = guard.invoke;
    assert.throws(
        () => {
            delegatedInvoke.call({ spoofed: 'top_window' }, sameOriginIframeScope, 'test_method', { value: 4 });
        },
        (err) => {
            assert.strictEqual(err.name, 'DOMException');
            return true;
        }
    );
});

test('Wire Attestation - authoritative host validation rejects subframes, mismatched documents, and ineligible origins', () => {
    const surface = {
        id: 'surf-attest',
        navigationState: 'committed',
        activeDocumentId: 'doc-auth-123',
        ipcEligible: true,
        currentOrigin: 'https://app.example',
    };

    // 1. Valid top-level frame matching surface active document
    const validFrame = {
        isTopLevel: true,
        pipelineId: 10,
        documentId: 'doc-auth-123',
    };
    const validRes = validateOutboundWireAttestation(surface, validFrame);
    assert.strictEqual(validRes.valid, true);

    // 2. Subframe attestation -> rejected with E_IPC_SUBFRAME_RESTRICTED
    const subframe = {
        isTopLevel: false,
        pipelineId: 11,
        documentId: 'doc-auth-123',
    };
    const subRes = validateOutboundWireAttestation(surface, subframe);
    assert.strictEqual(subRes.valid, false);
    assert.strictEqual(subRes.code, 'E_IPC_SUBFRAME_RESTRICTED');

    // 3. Document ID mismatch (stale frame from previous navigation) -> rejected
    const staleFrame = {
        isTopLevel: true,
        pipelineId: 10,
        documentId: 'doc-stale-old',
    };
    const staleRes = validateOutboundWireAttestation(surface, staleFrame);
    assert.strictEqual(staleRes.valid, false);
    assert.strictEqual(staleRes.code, 'E_IPC_DOCUMENT_ID_MISMATCH');

    // 4. Navigation provisional (not yet committed) -> rejected
    surface.navigationState = 'provisional';
    const provRes = validateOutboundWireAttestation(surface, validFrame);
    assert.strictEqual(provRes.valid, false);
    assert.strictEqual(provRes.code, 'E_IPC_NAVIGATION_NOT_COMMITTED');

    // 5. IPC ineligible origin (e.g. data: URL) -> rejected
    surface.navigationState = 'committed';
    surface.ipcEligible = false;
    const ineligRes = validateOutboundWireAttestation(surface, validFrame);
    assert.strictEqual(ineligRes.valid, false);
    assert.strictEqual(ineligRes.code, 'E_IPC_ORIGIN_INELIGIBLE');
});

test('Origin Derivation - enforces rules for data:, blob:, about: and Web Workers', () => {
    // 1. data: URLs -> strictly opaque 'null' origin, unconditionally IPC-ineligible
    const dataRes = deriveOriginForUrl('data:text/html,<h1>Hello</h1>');
    assert.strictEqual(dataRes.origin, 'null');
    assert.strictEqual(dataRes.ipcEligible, false);
    assert.strictEqual(dataRes.isOpaque, true);

    // 2. about:blank with creator context -> inherits creator origin & document ID
    const aboutBlankWithCreator = deriveOriginForUrl('about:blank', {
        origin: 'https://dashboard.example',
        documentId: 'doc-creator-99',
        ipcEligible: true,
    });
    assert.strictEqual(aboutBlankWithCreator.origin, 'https://dashboard.example');
    assert.strictEqual(aboutBlankWithCreator.documentId, 'doc-creator-99');
    assert.strictEqual(aboutBlankWithCreator.ipcEligible, true);
    assert.strictEqual(aboutBlankWithCreator.isOpaque, false);

    // 3. about:blank without creator context (top-level or untrusted) -> evaluates to null, ineligible
    const aboutBlankNoCreator = deriveOriginForUrl('about:blank');
    assert.strictEqual(aboutBlankNoCreator.origin, 'null');
    assert.strictEqual(aboutBlankNoCreator.ipcEligible, false);
    assert.strictEqual(aboutBlankNoCreator.isOpaque, true);

    // 4. blob: URL with valid creator -> inherits creator origin
    const blobRes = deriveOriginForUrl('blob:https://app.example/b8d73b32-3580-4966', {
        origin: 'https://app.example',
        documentId: 'doc-blob-1',
        ipcEligible: true,
    });
    assert.strictEqual(blobRes.origin, 'https://app.example');
    assert.strictEqual(blobRes.ipcEligible, true);

    // 5. blob: URL created from null/untrusted origin -> retains null origin and ineligible
    const blobNull = deriveOriginForUrl('blob:null/uuid-123', {
        origin: 'null',
        documentId: null,
        ipcEligible: false,
    });
    assert.strictEqual(blobNull.origin, 'null');
    assert.strictEqual(blobNull.ipcEligible, false);

    // 6. lotus-resource: -> lotus-resource://localhost, eligible
    const lotusResource = deriveOriginForUrl('lotus-resource://localhost/index.html');
    assert.strictEqual(lotusResource.origin, 'lotus-resource://localhost');
    assert.strictEqual(lotusResource.ipcEligible, true);

    // 7. Standard https: -> normalized origin, eligible
    const httpsRes = deriveOriginForUrl('https://my-app.io:9000/path?query=1');
    assert.strictEqual(httpsRes.origin, 'https://my-app.io:9000');
    assert.strictEqual(httpsRes.ipcEligible, true);
});
