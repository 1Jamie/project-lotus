const test = require('node:test');
const assert = require('node:assert');
const fs = require('fs');
const path = require('path');
const os = require('os');
const crypto = require('crypto');
const { generateShards, vfsPack } = require('../index.js');

test('VFS - generateShards produces correct XOR key shards', () => {
    const masterKey = crypto.randomBytes(32);
    const s1 = Buffer.from("LotusMasterFrameworkShard_v1_2026");
    const { s2, s3 } = generateShards(masterKey, s1);

    assert.strictEqual(s2.length, 32);
    assert.strictEqual(s3.length, 32);

    const reconstructed = Buffer.alloc(32);
    for (let i = 0; i < 32; i++) {
        reconstructed[i] = s1[i % s1.length] ^ s2[i] ^ s3[i];
    }

    assert.deepStrictEqual(reconstructed, masterKey);
});

test('VFS - vfsPack creates valid encrypted VFS blob and decrypts successfully', () => {
    const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'lotus-vfs-test-'));
    try {
        fs.writeFileSync(path.join(tmpDir, 'index.html'), '<html><body>Hello Lotus</body></html>');
        fs.mkdirSync(path.join(tmpDir, 'css'));
        fs.writeFileSync(path.join(tmpDir, 'css', 'style.css'), 'body { background: #000; }');

        const masterKey = crypto.randomBytes(32);
        const vfsBlob = vfsPack(tmpDir, masterKey);

        assert(vfsBlob.length > 12);
        assert.strictEqual(vfsBlob.subarray(0, 8).toString('utf8'), 'LOTUSVFS');

        const indexSize = vfsBlob.readUInt32LE(8);
        const indexJson = vfsBlob.subarray(12, 12 + indexSize).toString('utf8');
        const index = JSON.parse(indexJson);

        assert(index['index.html']);
        assert(index['css/style.css']);

        const dataOffset = 12 + indexSize;

        // Verify decryption for index.html
        const [offset, size] = index['index.html'];
        const block = vfsBlob.subarray(dataOffset + offset, dataOffset + offset + size);
        assert(block.length >= 12 + 16);

        const nonce = block.subarray(0, 12);
        const ciphertextAndTag = block.subarray(12);
        const ciphertext = ciphertextAndTag.subarray(0, ciphertextAndTag.length - 16);
        const authTag = ciphertextAndTag.subarray(ciphertextAndTag.length - 16);

        const decipher = crypto.createDecipheriv('aes-256-gcm', masterKey, nonce);
        decipher.setAuthTag(authTag);
        const decrypted = Buffer.concat([decipher.update(ciphertext), decipher.final()]);

        assert.strictEqual(decrypted.toString('utf8'), '<html><body>Hello Lotus</body></html>');
    } finally {
        fs.rmSync(tmpDir, { recursive: true, force: true });
    }
});

