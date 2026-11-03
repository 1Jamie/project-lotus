const test = require('node:test');
const assert = require('node:assert');
const { execSync } = require('child_process');
const fs = require('fs');
const path = require('path');
const os = require('os');

const cliPath = path.resolve(__dirname, '..', 'bin', 'lotus.js');

test('CLI - lotus --help runs and lists available commands', () => {
    const output = execSync(`node "${cliPath}" --help`, { encoding: 'utf8' });
    assert(output.includes('dev [entry]'));
    assert(output.includes('clean'));
    assert(output.includes('build'));
    assert(output.includes('init [options] [projectName]'));
    assert(output.includes('fetch [options]'));
});

test('CLI - lotus init generates lotus.config.json without plain resources', () => {
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'lotus-init-cli-'));
    try {
        execSync(
            `node "${cliPath}" init test-app --name "Test App" --description "Test" --author "Tester" --license "MIT" --app-version "1.0.0" --homepage "https://example.com"`,
            { cwd: tmp, stdio: 'pipe' }
        );

        const configPath = path.join(tmp, 'test-app', 'lotus.config.json');
        assert(fs.existsSync(configPath));
        const cfg = JSON.parse(fs.readFileSync(configPath, 'utf8'));
        assert.strictEqual(cfg.name, 'Test App');
        assert.strictEqual(cfg.version, '1.0.0');
        assert.strictEqual(cfg.resources, undefined);

        const mainPath = path.join(tmp, 'test-app', 'main.js');
        assert(fs.existsSync(mainPath));
        const mainContent = fs.readFileSync(mainPath, 'utf8');
        assert(mainContent.includes('app.initVfs();'));
        assert(mainContent.includes('app.warmup();'));
    } finally {
        fs.rmSync(tmp, { recursive: true, force: true });
    }
});

test('CLI - lotus fetch command executes successfully', () => {
    const output = execSync(`node "${cliPath}" fetch`, { encoding: 'utf8' });
    assert(output.includes('fetch completed') || output.includes('Binary already exists'));
});
