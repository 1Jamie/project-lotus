const test = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const path = require('path');
const os = require('os');
const ResEdit = require('resedit');
const { verifyInjectedData } = require('../index.js');

test('Injection - verifyInjectedData validates PE binary with SEA blob and custom Lotus resources', () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'lotus-pe-test-'));
    try {
        const pePath = path.join(tmpDir, 'test-app.exe');

        // Create a minimal valid PE executable using resedit's NtExecutable
        const exe = ResEdit.NtExecutable.createEmpty();
        const res = ResEdit.NtExecutableResource.from(exe);

        // Inject simulated NODE_SEA_BLOB
        res.entries.push({
            type: 10, // RT_RCDATA
            id: 'NODE_SEA_BLOB',
            lang: 1033,
            bin: Buffer.from('SIMULATED_SEA_BLOB_PAYLOAD')
        });

        // Inject Lotus encryption shards and VFS
        const s1 = Buffer.alloc(32, 1);
        const s2 = Buffer.alloc(32, 2);
        const vfs = Buffer.from('LOTUSVFS\x02\x00\x00\x00{}DATA');

        res.entries.push({ type: 10, id: 'LOTUS_APP_S1', lang: 1033, bin: s1 });
        res.entries.push({ type: 10, id: 'LOTUS_APP_S2', lang: 1033, bin: s2 });
        res.entries.push({ type: 10, id: 'LOTUS_VFS', lang: 1033, bin: vfs });

        res.outputResource(exe);
        fs.writeFileSync(pePath, Buffer.from(exe.generate()));

        // Verification should succeed with exact expected sizes
        assert.doesNotThrow(() => {
            verifyInjectedData(pePath, true, {
                LOTUS_APP_S1: s1.length,
                LOTUS_APP_S2: s2.length,
                LOTUS_VFS: vfs.length
            });
        });

        // Verification should fail if a required section is missing or too small
        assert.throws(() => {
            verifyInjectedData(pePath, true, {
                LOTUS_APP_S1: 32,
                LOTUS_MISSING_SECTION: 10
            });
        }, /missing LOTUS_MISSING_SECTION/);
    } finally {
        fs.rmSync(tmpDir, { recursive: true, force: true });
    }
});

test('Injection - verifyInjectedData rejects PE binary without NODE_SEA_BLOB', () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'lotus-pe-fail-'));
    try {
        const pePath = path.join(tmpDir, 'test-empty.exe');
        const exe = ResEdit.NtExecutable.createEmpty();
        const res = ResEdit.NtExecutableResource.from(exe);
        res.outputResource(exe);
        fs.writeFileSync(pePath, Buffer.from(exe.generate()));

        assert.throws(() => {
            verifyInjectedData(pePath, true, null);
        }, /missing NODE_SEA_BLOB resource/);
    } finally {
        fs.rmSync(tmpDir, { recursive: true, force: true });
    }
});
