const { test } = require('node:test');
const assert = require('node:assert');
const path = require('node:path');
const http = require('node:http');

const lotusCore = require(path.join(__dirname, '..', '..', 'lotus-core', 'lotus.js'));
const {
    NetworkBroker,
    HostCookieStore,
    isAllowedPublicIp,
    normalizeOrigin,
    parseCookieString,
    MAX_COOKIE_SEQ_GAP,
    MAX_PENDING_COOKIE_MUTATIONS_PER_DOC,
    MAX_PENDING_COOKIE_BYTES_PER_DOC,
    MAX_SINGLE_COOKIE_STRING_BYTES,
    MAX_SINGLE_COOKIE_ATTR_BYTES,
    MAX_PENDING_FETCH_BARRIERS_PER_DOC
} = lotusCore.networkBroker;

test('Network Broker - anti-SSRF IP classification blocks private, loopback, and tunneling ranges', () => {
    // 1. Loopback addresses
    assert.strictEqual(isAllowedPublicIp('127.0.0.1', false), false, '127.0.0.1 must be blocked');
    assert.strictEqual(isAllowedPublicIp('127.0.0.2', false), false, '127.0.0.2 must be blocked');
    assert.strictEqual(isAllowedPublicIp('::1', false), false, '::1 must be blocked');

    // 2. Private RFC 1918 addresses
    assert.strictEqual(isAllowedPublicIp('10.0.0.1', false), false, '10.0.0.1 must be blocked');
    assert.strictEqual(isAllowedPublicIp('172.16.0.1', false), false, '172.16.0.1 must be blocked');
    assert.strictEqual(isAllowedPublicIp('192.168.1.1', false), false, '192.168.1.1 must be blocked');

    // 3. Link-Local & AWS metadata
    assert.strictEqual(isAllowedPublicIp('169.254.169.254', false), false, 'AWS metadata 169.254.169.254 must be blocked');
    assert.strictEqual(isAllowedPublicIp('fe80::1', false), false, 'IPv6 link-local must be blocked');

    // 4. Carrier-Grade NAT (RFC 6598)
    assert.strictEqual(isAllowedPublicIp('100.64.0.1', false), false, 'Carrier-grade NAT 100.64.0.1 must be blocked');

    // 5. IPv4-mapped IPv6
    assert.strictEqual(isAllowedPublicIp('::ffff:127.0.0.1', false), false, 'IPv4-mapped loopback must be blocked');
    assert.strictEqual(isAllowedPublicIp('::ffff:192.168.1.1', false), false, 'IPv4-mapped private must be blocked');
    assert.strictEqual(isAllowedPublicIp('::ffff:169.254.169.254', false), false, 'IPv4-mapped link-local must be blocked');

    // 6. NAT64 prefix (64:ff9b::/96)
    assert.strictEqual(isAllowedPublicIp('64:ff9b::127.0.0.1', false), false, 'NAT64 prefix must be blocked');

    // 7. Teredo (2001::/32) and 6to4 (2002::/16)
    assert.strictEqual(isAllowedPublicIp('2001:0000:4136:e378:8000:63bf:3fff:fdd2', false), false, 'Teredo prefix must be blocked');
    assert.strictEqual(isAllowedPublicIp('2002:c0a8:0101::', false), false, '6to4 prefix must be blocked');

    // 8. Multicast
    assert.strictEqual(isAllowedPublicIp('224.0.0.1', false), false, 'IPv4 multicast must be blocked');
    assert.strictEqual(isAllowedPublicIp('ff02::1', false), false, 'IPv6 multicast must be blocked');

    // 9. Valid Public Unicast IPs
    assert.strictEqual(isAllowedPublicIp('8.8.8.8', false), true, 'Google DNS 8.8.8.8 must be allowed');
    assert.strictEqual(isAllowedPublicIp('1.1.1.1', false), true, 'Cloudflare DNS 1.1.1.1 must be allowed');
    assert.strictEqual(isAllowedPublicIp('2606:4700:4700::1111', false), true, 'Cloudflare IPv6 must be allowed');

    // 10. Localhost opt-in
    assert.strictEqual(isAllowedPublicIp('127.0.0.1', true), true, 'Localhost allowed when opt-in is enabled');
    assert.strictEqual(isAllowedPublicIp('10.0.0.1', true), false, 'Private RFC 1918 still blocked even with localhost opt-in');
});

test('Network Broker - userinfo rejection and origin normalization', () => {
    assert.strictEqual(normalizeOrigin('https://example.com:8443/test'), 'https://example.com:8443');
    assert.strictEqual(normalizeOrigin('http://foo.bar/baz'), 'http://foo.bar');
    assert.strictEqual(normalizeOrigin('ftp://evil.com'), null);
    assert.strictEqual(normalizeOrigin('file:///etc/passwd'), null);

    const broker = new NetworkBroker();
    const surface = { id: 'surf-1', navigationState: 'committed', activeDocumentId: 'doc-1', currentOrigin: 'https://example.com' };

    // Request with userinfo in URL should reject with E_BROKER_USERINFO_FORBIDDEN
    return assert.rejects(
        broker.handleBrokerFetchRequest(surface, 'doc-1', {
            url: 'https://admin:secret@example.com/api',
            requiredCookieSequence: 0
        }),
        (err) => {
            assert.strictEqual(err.code, 'E_BROKER_USERINFO_FORBIDDEN');
            return true;
        }
    );
});

test('Network Broker - host cookie store RFC 6265 parsing, domain/path matching and HttpOnly separation', () => {
    const store = new HostCookieStore();
    const surfaceId = 'surf-1';
    const topOrigin = 'https://example.com';

    // Parse valid cookie with attributes
    const c1 = parseCookieString('session=abc123; Path=/api; Domain=example.com; Secure; HttpOnly; SameSite=Strict');
    assert(c1);
    assert.strictEqual(c1.name, 'session');
    assert.strictEqual(c1.value, 'abc123');
    assert.strictEqual(c1.path, '/api');
    assert.strictEqual(c1.domain, 'example.com');
    assert.strictEqual(c1.secure, true);
    assert.strictEqual(c1.httpOnly, true);
    assert.strictEqual(c1.sameSite, 'Strict');

    // Store c1
    store.setCookie(surfaceId, topOrigin, c1);

    // Matching path: /api/users
    let header = store.getCookieHeader(surfaceId, topOrigin, 'https://example.com/api/users');
    assert.strictEqual(header, 'session=abc123');

    // Non-matching path: /other
    header = store.getCookieHeader(surfaceId, topOrigin, 'https://example.com/other');
    assert.strictEqual(header, '');

    // Insecure protocol for secure cookie
    header = store.getCookieHeader(surfaceId, topOrigin, 'http://example.com/api/users');
    assert.strictEqual(header, '');

    // Subdomain matching
    header = store.getCookieHeader(surfaceId, topOrigin, 'https://sub.example.com/api/users');
    assert.strictEqual(header, 'session=abc123');

    // Script attempted mutation with HttpOnly must be rejected from document.cookie
    const broker = new NetworkBroker();
    const surface = { id: surfaceId, navigationState: 'committed', activeDocumentId: 'doc-1', currentOrigin: topOrigin };
    broker.onCookieSetEnvelope(surface, 'doc-1', 1, 'bad=evil; HttpOnly', {});

    // Broker cookie store must NOT contain HttpOnly cookie from script
    const scriptCookie = broker.cookieStore.getCookieHeader(surfaceId, topOrigin, 'https://example.com/');
    assert.strictEqual(scriptCookie, '');
});

test('Network Broker - synchronous sequence snapshot and happens-before fetch barrier ordering', async () => {
    const broker = new NetworkBroker({ allowLocalhost: true });
    const surface = {
        id: 'surf-test',
        navigationState: 'committed',
        activeDocumentId: 'doc-100',
        currentOrigin: 'http://127.0.0.1:0'
    };

    // Spin up local test HTTP server
    const server = http.createServer((req, res) => {
        res.setHeader('Set-Cookie', 'server_token=xyz789; Path=/; SameSite=Lax');
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ receivedCookie: req.headers['cookie'] || '' }));
    });

    await new Promise(r => server.listen(0, '127.0.0.1', r));
    const port = server.address().port;
    const testUrl = `http://127.0.0.1:${port}/test`;
    surface.currentOrigin = `http://127.0.0.1:${port}`;

    try {
        // Step 1: Script sets cookie sequence 1
        broker.onCookieSetEnvelope(surface, 'doc-100', 1, 'client_auth=secret42; Path=/', {});

        // Step 2: Immediate fetch requesting sequence 1
        const res1 = await broker.handleBrokerFetchRequest(surface, 'doc-100', {
            url: testUrl,
            requiredCookieSequence: 1
        });

        assert.strictEqual(res1.status, 200);
        const data1 = JSON.parse(res1.body.toString());
        assert.strictEqual(data1.receivedCookie, 'client_auth=secret42', 'Fetch must carry client_auth cookie');

        // Step 3: Response set-cookie was ingested into broker store
        assert.strictEqual(res1.clientSetCookies.length, 1);
        assert.strictEqual(res1.clientSetCookies[0].name, 'server_token');

        // Step 4: Next fetch carries both client and server cookies
        const res2 = await broker.handleBrokerFetchRequest(surface, 'doc-100', {
            url: testUrl,
            requiredCookieSequence: 1
        });
        const data2 = JSON.parse(res2.body.toString());
        assert(data2.receivedCookie.includes('client_auth=secret42'));
        assert(data2.receivedCookie.includes('server_token=xyz789'));
    } finally {
        broker.purgeDocumentSyncState('surf-reorder', 'doc-200');
        server.close();
    }
});

test('Network Broker - out-of-order cookie reassembly and fetch barrier unblocking', async () => {
    const broker = new NetworkBroker({ allowLocalhost: true });
    const surface = {
        id: 'surf-reorder',
        navigationState: 'committed',
        activeDocumentId: 'doc-200',
        currentOrigin: 'http://127.0.0.1:0',
        processedCookieSequence: 0
    };

    const server = http.createServer((req, res) => {
        res.writeHead(200);
        res.end(JSON.stringify({ cookie: req.headers['cookie'] || '' }));
    });
    await new Promise(r => server.listen(0, '127.0.0.1', r));
    const port = server.address().port;
    const testUrl = `http://127.0.0.1:${port}/`;
    surface.currentOrigin = `http://127.0.0.1:${port}`;

    try {
        // Issue fetch requiring sequence 2 BEFORE sequence 1 or 2 arrive (held in barrier)
        const fetchPromise = broker.handleBrokerFetchRequest(surface, 'doc-200', {
            url: testUrl,
            requiredCookieSequence: 2
        });

        // Mutate sequence 2 out of order (buffered in reassembly)
        broker.onCookieSetEnvelope(surface, 'doc-200', 2, 'second=val2; Path=/', {});
        assert.strictEqual(surface.processedCookieSequence, 0, 'Cannot advance past missing sequence 1');

        // Now deliver missing sequence 1
        broker.onCookieSetEnvelope(surface, 'doc-200', 1, 'first=val1; Path=/', {});
        assert.strictEqual(surface.processedCookieSequence, 2, 'Must advance to 2 once contiguous');

        // Barrier should now be unblocked and fetch resolves
        const res = await fetchPromise;
        const data = JSON.parse(res.body.toString());
        assert(data.cookie.includes('first=val1'));
        assert(data.cookie.includes('second=val2'));
    } finally {
        broker.purgeDocumentSyncState('surf-reorder', 'doc-200');
        server.close();
    }
});

test('Network Broker - rejected cookie assignments advance sequence without store bloat', async () => {
    const broker = new NetworkBroker({ allowLocalhost: true });
    const surface = {
        id: 'surf-invalid',
        navigationState: 'committed',
        activeDocumentId: 'doc-300',
        currentOrigin: 'http://127.0.0.1:0'
    };

    const server = http.createServer((req, res) => {
        res.writeHead(200);
        res.end(JSON.stringify({ cookie: req.headers['cookie'] || '' }));
    });
    await new Promise(r => server.listen(0, '127.0.0.1', r));
    const port = server.address().port;
    const testUrl = `http://127.0.0.1:${port}/`;
    surface.currentOrigin = `http://127.0.0.1:${port}`;

    try {
        // Deliver sequence 1: valid
        broker.onCookieSetEnvelope(surface, 'doc-300', 1, 'valid1=yes; Path=/', {});

        // Deliver sequence 2: INVALID (malformed / disallowed characters)
        broker.onCookieSetEnvelope(surface, 'doc-300', 2, 'bad cookie name!=val; Path=/', {});

        // Deliver sequence 3: valid
        broker.onCookieSetEnvelope(surface, 'doc-300', 3, 'valid3=yes; Path=/', {});

        // Sequence must advance to 3
        assert.strictEqual(surface.processedCookieSequence, 3);

        // Fetch requiring sequence 3 should succeed immediately with only valid cookies
        const res = await broker.handleBrokerFetchRequest(surface, 'doc-300', {
            url: testUrl,
            requiredCookieSequence: 3
        });
        const data = JSON.parse(res.body.toString());
        assert(data.cookie.includes('valid1=yes'));
        assert(data.cookie.includes('valid3=yes'));
        assert(!data.cookie.includes('bad'));
    } finally {
        broker.purgeDocumentSyncState('surf-reorder', 'doc-200');
        server.close();
    }
});

test('Network Broker - adversarial queue bounds and deterministic overflow fault handling', async () => {
    const broker = new NetworkBroker({ allowLocalhost: true });
    const surface = {
        id: 'surf-adv',
        navigationState: 'committed',
        activeDocumentId: 'doc-adv',
        currentOrigin: 'https://example.com'
    };

    // 1. Extreme sequence gap (> 64) triggers E_BROKER_COOKIE_QUEUE_OVERFLOW
    broker.onCookieSetEnvelope(surface, 'doc-adv', 100, 'oversized_gap=true', {});
    assert.strictEqual(surface.cookieSyncFaulted, true, 'Surface must fault on gap > 64');

    // 2. Fetch on faulted surface rejects with E_BROKER_COOKIE_SYNC_FAULT
    await assert.rejects(
        broker.handleBrokerFetchRequest(surface, 'doc-adv', {
            url: 'https://example.com/test',
            requiredCookieSequence: 10
        }),
        (err) => {
            assert.strictEqual(err.code, 'E_BROKER_COOKIE_SYNC_FAULT');
            return true;
        }
    );

    // 3. Navigation to new document resets and recovers fault
    broker.purgeDocumentSyncState('surf-adv', 'doc-adv');
    surface.activeDocumentId = 'doc-fresh';
    surface.cookieSyncFaulted = false;
    surface.processedCookieSequence = 0;

    // Verify recovery on new document
    broker.onCookieSetEnvelope(surface, 'doc-fresh', 1, 'recovered=true; Path=/', {});
    assert.strictEqual(surface.processedCookieSequence, 1);
    assert.strictEqual(surface.cookieSyncFaulted, false);
});
