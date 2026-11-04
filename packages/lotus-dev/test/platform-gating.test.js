const { test } = require('node:test');
const assert = require('node:assert');
const path = require('node:path');

// Require directly from lotus-core
const lotusCore = require(path.join(__dirname, '..', '..', 'lotus-core', 'lotus.js'));
const { validatePlatformSecurityIsolation, detectLinuxThemeNode } = lotusCore;

test('Platform Gating - allows un-isolated or opted-down secure surfaces', () => {
    // secure: false should never throw regardless of platform
    assert.doesNotThrow(() => {
        validatePlatformSecurityIsolation({ secure: false });
    });
    assert.doesNotThrow(() => {
        validatePlatformSecurityIsolation({});
    });

    // secure: true with explicit separateProcess: false (in-process hosting downgrade) should not throw platform gating error
    assert.doesNotThrow(() => {
        validatePlatformSecurityIsolation({ secure: true, separateProcess: false });
    });
});

test('Platform Gating - throws E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM on unsupported platforms and inside Flatpak', () => {
    const originalPlatform = process.platform;
    const originalFlatpakId = process.env.FLATPAK_ID;
    try {
        // Simulate darwin (macOS) with both sandbox: true and secure: true
        Object.defineProperty(process, 'platform', { value: 'darwin', configurable: true });
        assert.throws(() => {
            validatePlatformSecurityIsolation({ sandbox: true });
        }, (err) => {
            assert.strictEqual(err.code, 'E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM');
            assert.match(err.message, /darwin/);
            return true;
        });

        // Simulate win32 (Windows)
        Object.defineProperty(process, 'platform', { value: 'win32', configurable: true });
        assert.throws(() => {
            validatePlatformSecurityIsolation({ sandbox: true, separateProcess: true });
        }, (err) => {
            assert.strictEqual(err.code, 'E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM');
            assert.match(err.message, /win32/);
            return true;
        });

        // Simulate Linux inside Flatpak container (FLATPAK_ID set)
        Object.defineProperty(process, 'platform', { value: 'linux', configurable: true });
        process.env.FLATPAK_ID = 'org.example.LotusApp';
        assert.throws(() => {
            validatePlatformSecurityIsolation({ sandbox: true });
        }, (err) => {
            assert.strictEqual(err.code, 'E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM');
            assert.match(err.message, /Flatpak/);
            return true;
        });
    } finally {
        Object.defineProperty(process, 'platform', { value: originalPlatform, configurable: true });
        if (originalFlatpakId === undefined) {
            delete process.env.FLATPAK_ID;
        } else {
            process.env.FLATPAK_ID = originalFlatpakId;
        }
    }
});

test('Bridge Origin Guard - window.lotus is undefined when page origin is not in trustedOrigins', () => {
    const vm = require('node:vm');
    const { hostIpcPolicy } = lotusCore;

    const script = hostIpcPolicy.generateSecureBridgeScript({
        surfaceId: 'pane-docs',
        documentId: 'doc-1',
        bootstrapSecret: 'secret-abc',
        enginePort: 9000,
        trustedOrigins: ['https://docs.example.com'],
    });

    // 1. Untrusted origin -> window.lotus must remain undefined
    const untrustedWindow = vm.createContext({
        location: { origin: 'https://untrusted.example.org', protocol: 'https:', host: 'untrusted.example.org', href: 'https://untrusted.example.org/' },
    });
    untrustedWindow.window = untrustedWindow;
    untrustedWindow.self = untrustedWindow;
    untrustedWindow.parent = untrustedWindow;
    untrustedWindow.top = untrustedWindow;

    vm.runInContext(script, untrustedWindow);
    assert.strictEqual(untrustedWindow.window.lotus, undefined, 'window.lotus must be undefined on untrusted origins');

    // 2. Trusted origin -> window.lotus is defined and frozen
    const trustedWindow = vm.createContext({
        location: { origin: 'https://docs.example.com', protocol: 'https:', host: 'docs.example.com', href: 'https://docs.example.com/' },
    });
    trustedWindow.window = trustedWindow;
    trustedWindow.self = trustedWindow;
    trustedWindow.parent = trustedWindow;
    trustedWindow.top = trustedWindow;

    vm.runInContext(script, trustedWindow);
    assert.ok(trustedWindow.window.lotus, 'window.lotus must be defined on trusted origins');
    assert.strictEqual(typeof trustedWindow.window.lotus.invoke, 'function');
});

test('Theme Detection - detectLinuxThemeNode executes safely without error', () => {
    const theme = detectLinuxThemeNode();
    // On Linux without GNOME or in headless CI, it returns null or 'light'/'dark'
    if (theme !== null) {
        assert.ok(theme === 'light' || theme === 'dark');
    }
});
