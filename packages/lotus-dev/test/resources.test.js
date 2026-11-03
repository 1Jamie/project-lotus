const test = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const path = require('path');
const os = require('os');
const { stripGlob, classifyResourceVsAssetDir, resolveAssetDir } = require('../index.js');

test('Resources - stripGlob correctly strips glob patterns without truncating directories', () => {
    // Normal directory
    assert.strictEqual(stripGlob('./ui'), './ui');
    assert.strictEqual(stripGlob('ui/'), 'ui/');

    // Glob following a slash (must NOT truncate to '.')
    assert.strictEqual(stripGlob('./ui/**/*'), './ui');
    assert.strictEqual(stripGlob('ui/*'), 'ui');

    // Glob inside a filename
    assert.strictEqual(stripGlob('./ui/bundle-*.js'), './ui');
    assert.strictEqual(stripGlob('dist/*.css'), 'dist');
});

test('Resources - classifyResourceVsAssetDir categorizes resource paths accurately', () => {
    const root = '/my-project';
    const assetDir = '/my-project/ui';

    // Same path or subpath of asset directory
    assert.strictEqual(classifyResourceVsAssetDir('./ui', assetDir, root), 'same_or_inside');
    assert.strictEqual(classifyResourceVsAssetDir('ui', assetDir, root), 'same_or_inside');
    assert.strictEqual(classifyResourceVsAssetDir('./ui/**/*', assetDir, root), 'same_or_inside');
    assert.strictEqual(classifyResourceVsAssetDir('ui/nested/file.png', assetDir, root), 'same_or_inside');

    // Parent directory of asset directory (e.g. uiDir is dist/renderer and resources is dist)
    const distAssetDir = '/my-project/dist/renderer';
    assert.strictEqual(classifyResourceVsAssetDir('dist', distAssetDir, root), 'parent_of_asset');
    assert.strictEqual(classifyResourceVsAssetDir('./dist', distAssetDir, root), 'parent_of_asset');
    assert.strictEqual(classifyResourceVsAssetDir('.', distAssetDir, root), 'parent_of_asset');

    // Unrelated path
    assert.strictEqual(classifyResourceVsAssetDir('assets', assetDir, root), 'unrelated');
    assert.strictEqual(classifyResourceVsAssetDir('./extra/data.json', assetDir, root), 'unrelated');
});

test('Resources - resolveAssetDir prioritizes explicit config and handles fallbacks', () => {
    const tmpProject = fs.mkdtempSync(path.join(os.tmpdir(), 'lotus-asset-test-'));
    try {
        fs.mkdirSync(path.join(tmpProject, 'ui'));
        fs.mkdirSync(path.join(tmpProject, 'renderer'));

        // 1. Explicit config takes precedence
        const explicitConfig = { uiDir: 'renderer' };
        assert.strictEqual(resolveAssetDir(explicitConfig, tmpProject), path.join(tmpProject, 'renderer'));

        // 2. Missing explicit config throws
        assert.throws(() => {
            resolveAssetDir({ uiDir: 'nonexistent' }, tmpProject);
        }, /Configured asset directory "nonexistent"/);

        // 3. Candidate fallback defaults to 'ui' when both ui and renderer exist
        const candidateConfig = {};
        assert.strictEqual(resolveAssetDir(candidateConfig, tmpProject), path.join(tmpProject, 'ui'));

        // 4. Protection against unbuilt public directory when build script exists
        const unbuiltProject = fs.mkdtempSync(path.join(os.tmpdir(), 'lotus-unbuilt-test-'));
        try {
            fs.mkdirSync(path.join(unbuiltProject, 'public'));
            fs.writeFileSync(path.join(unbuiltProject, 'package.json'), JSON.stringify({
                scripts: { build: 'vite build' }
            }));

            assert.throws(() => {
                resolveAssetDir({}, unbuiltProject, { encrypt: true });
            }, /No built frontend directory .* found, only "public\/"/);
        } finally {
            fs.rmSync(unbuiltProject, { recursive: true, force: true });
        }
    } finally {
        fs.rmSync(tmpProject, { recursive: true, force: true });
    }
});

