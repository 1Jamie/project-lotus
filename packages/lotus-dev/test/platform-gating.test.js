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

test('Platform Gating - throws E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM on unsupported platforms', () => {
    const originalPlatform = process.platform;
    try {
        // Simulate darwin (macOS - future target without verified implementation)
        Object.defineProperty(process, 'platform', { value: 'darwin', configurable: true });
        assert.throws(() => {
            validatePlatformSecurityIsolation({ secure: true }); // default separateProcess: true
        }, (err) => {
            assert.strictEqual(err.code, 'E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM');
            assert.match(err.message, /darwin/);
            return true;
        });

        // Simulate win32 (Windows - planned target without verified implementation)
        Object.defineProperty(process, 'platform', { value: 'win32', configurable: true });
        assert.throws(() => {
            validatePlatformSecurityIsolation({ secure: true, separateProcess: true });
        }, (err) => {
            assert.strictEqual(err.code, 'E_SECURITY_ISOLATION_UNSUPPORTED_PLATFORM');
            assert.match(err.message, /win32/);
            return true;
        });
    } finally {
        Object.defineProperty(process, 'platform', { value: originalPlatform, configurable: true });
    }
});

test('Theme Detection - detectLinuxThemeNode executes safely without error', () => {
    const theme = detectLinuxThemeNode();
    // On Linux without GNOME or in headless CI, it returns null or 'light'/'dark'
    if (theme !== null) {
        assert.ok(theme === 'light' || theme === 'dark');
    }
});
