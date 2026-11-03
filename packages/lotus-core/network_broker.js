//! packages/lotus-core/network_broker.js
//! Hardened Anti-SSRF Node Network Broker & Host-Authoritative Cookie Store.

const http = require('node:http');
const https = require('node:https');
const dns = require('node:dns');
const ipaddr = require('ipaddr.js');

// Numerical Bounds from Architecture Specification
const MAX_COOKIE_SEQ_GAP = 64;
const MAX_PENDING_COOKIE_MUTATIONS_PER_DOC = 64;
const MAX_PENDING_COOKIE_BYTES_PER_DOC = 65536; // 64 KiB
const MAX_SINGLE_COOKIE_STRING_BYTES = 4096; // 4 KiB
const MAX_SINGLE_COOKIE_ATTR_BYTES = 1024; // 1 KiB
const MAX_PENDING_FETCH_BARRIERS_PER_DOC = 32;
const MAX_PENDING_FETCH_BYTES_PER_DOC = 1048576; // 1 MiB
const MAX_UNRESOLVED_COOKIE_GAP_MS = 2000; // 2.0s
const MAX_FETCH_BARRIER_TIMEOUT_MS = 5000; // 5.0s
const MAX_RESPONSE_BODY_BYTES = 64 * 1024 * 1024; // 64 MiB hard ceiling
const MAX_CONCURRENT_REQUESTS = 32; // 32 concurrent request limit
const REQUEST_TIMEOUT_MS = 30000; // 30s per-request timeout
const MAX_REDIRECT_HOPS = 5; // Up to 5 validated redirect hops

const HOP_BY_HOP_HEADERS = new Set([
    'connection',
    'keep-alive',
    'proxy-authenticate',
    'proxy-authorization',
    'te',
    'trailer',
    'transfer-encoding',
    'upgrade',
    'host',
    'cookie'
]);

/**
 * Validates whether an IP address is a safe, routable public unicast address.
 * Rejects loopback, private, link-local, carrier-grade NAT, multicast, reserved,
 * NAT64, 6to4, Teredo, and IPv4-mapped IPv6 ranges.
 */
function isAllowedPublicIp(ipString, allowLocalhost = false) {
    try {
        let addr = ipaddr.parse(ipString);

        // Unwrap IPv4-mapped IPv6 address (::ffff:1.2.3.4)
        if (addr.kind() === 'ipv6') {
            if (addr.isIPv4MappedAddress()) {
                addr = addr.toIPv4Address();
            }
        }

        if (addr.kind() === 'ipv4') {
            const range = addr.range();
            if (allowLocalhost && range === 'loopback') {
                return true;
            }
            // Only public unicast IPv4 addresses are permitted
            return range === 'unicast';
        }

        if (addr.kind() === 'ipv6') {
            const range = addr.range();
            if (allowLocalhost && range === 'loopback') {
                return true;
            }

            // Reject RFC 6052 / 6146 NAT64 prefix (64:ff9b::/96)
            const parts = addr.parts;
            if (parts[0] === 0x0064 && parts[1] === 0xff9b && parts[2] === 0 && parts[3] === 0 && parts[4] === 0 && parts[5] === 0) {
                return false;
            }

            // Reject RFC 4380 Teredo prefix (2001::/32)
            if (parts[0] === 0x2001 && parts[1] === 0x0000) {
                return false;
            }

            // Reject RFC 3056 6to4 prefix (2002::/16)
            if (parts[0] === 0x2002) {
                return false;
            }

            return range === 'unicast';
        }

        return false;
    } catch {
        return false;
    }
}

/**
 * Normalizes an origin string according to WHATWG rules.
 */
function normalizeOrigin(urlStr) {
    try {
        const u = new URL(urlStr);
        if (u.protocol !== 'http:' && u.protocol !== 'https:') {
            return null;
        }
        return u.origin.toLowerCase();
    } catch {
        return null;
    }
}

/**
 * Measures serialized size in bytes of cookie attributes object.
 */
function measureAttributesBytes(attributes) {
    if (!attributes || typeof attributes !== 'object') return 0;
    try {
        return Buffer.byteLength(JSON.stringify(attributes), 'utf8');
    } catch {
        return 0;
    }
}

/**
 * Parses RFC 6265 Set-Cookie and document.cookie strings.
 */
function parseCookieString(rawCookie) {
    if (!rawCookie || typeof rawCookie !== 'string') return null;
    const parts = rawCookie.split(';');
    const first = parts[0].trim();
    const eqIdx = first.indexOf('=');
    if (eqIdx <= 0) return null;

    const name = first.slice(0, eqIdx).trim();
    const value = first.slice(eqIdx + 1).trim();

    // Validate token characters
    if (/[\x00-\x1F\x7F\s,;"\\]/.test(name)) return null;

    const parsed = {
        name,
        value,
        path: '/',
        domain: null,
        expires: null,
        maxAge: null,
        secure: false,
        httpOnly: false,
        sameSite: 'Lax'
    };

    for (let i = 1; i < parts.length; i++) {
        const part = parts[i].trim();
        if (!part) continue;
        const attrEq = part.indexOf('=');
        const attrName = (attrEq > 0 ? part.slice(0, attrEq) : part).trim().toLowerCase();
        const attrVal = attrEq > 0 ? part.slice(attrEq + 1).trim() : '';

        switch (attrName) {
            case 'path':
                if (attrVal) parsed.path = attrVal.startsWith('/') ? attrVal : '/' + attrVal;
                break;
            case 'domain':
                if (attrVal) {
                    parsed.domain = attrVal.toLowerCase().replace(/^\./, '');
                }
                break;
            case 'expires':
                if (attrVal) {
                    const d = new Date(attrVal);
                    if (!isNaN(d.getTime())) parsed.expires = d;
                }
                break;
            case 'max-age':
                const sec = parseInt(attrVal, 10);
                if (!isNaN(sec)) {
                    parsed.maxAge = sec;
                    parsed.expires = new Date(Date.now() + sec * 1000);
                }
                break;
            case 'secure':
                parsed.secure = true;
                break;
            case 'httponly':
                parsed.httpOnly = true;
                break;
            case 'samesite':
                const s = attrVal.toLowerCase();
                if (s === 'strict') parsed.sameSite = 'Strict';
                else if (s === 'none') parsed.sameSite = 'None';
                else parsed.sameSite = 'Lax';
                break;
        }
    }

    return parsed;
}

/**
 * Host-Authoritative Cookie Store partitioned by (surfaceId, topLevelOrigin).
 */
class HostCookieStore {
    constructor() {
        this._partitions = new Map(); // `${surfaceId}:${topLevelOrigin}` -> Map(cookieKey -> cookieObj)
    }

    _getPartition(surfaceId, topLevelOrigin) {
        const key = `${surfaceId}:${topLevelOrigin}`;
        let partition = this._partitions.get(key);
        if (!partition) {
            partition = new Map();
            this._partitions.set(key, partition);
        }
        return partition;
    }

    setCookie(surfaceId, topLevelOrigin, cookieObj) {
        if (!cookieObj || !cookieObj.name) return;
        const partition = this._getPartition(surfaceId, topLevelOrigin);
        const cookieKey = `${cookieObj.name}#${cookieObj.domain || ''}#${cookieObj.path || '/'}`;

        // Check expiry
        if (cookieObj.expires && cookieObj.expires.getTime() <= Date.now()) {
            partition.delete(cookieKey);
            return;
        }
        if (cookieObj.maxAge !== null && cookieObj.maxAge <= 0) {
            partition.delete(cookieKey);
            return;
        }

        partition.set(cookieKey, cookieObj);
    }

    getCookieHeader(surfaceId, topLevelOrigin, targetUrl) {
        const partition = this._getPartition(surfaceId, topLevelOrigin);
        let u;
        try {
            u = new URL(targetUrl);
        } catch {
            return '';
        }

        const now = Date.now();
        const matched = [];
        const targetHost = u.hostname.toLowerCase();
        const targetPath = u.pathname || '/';
        const isSecureProtocol = u.protocol === 'https:';

        for (const [key, c] of partition.entries()) {
            // Expiry check
            if (c.expires && c.expires.getTime() <= now) {
                partition.delete(key);
                continue;
            }

            // Secure check
            if (c.secure && !isSecureProtocol) {
                continue;
            }

            // Domain match
            if (c.domain) {
                if (targetHost !== c.domain && !targetHost.endsWith('.' + c.domain)) {
                    continue;
                }
            }

            // Path match
            if (c.path) {
                if (!targetPath.startsWith(c.path)) {
                    continue;
                }
            }

            matched.push(`${c.name}=${c.value}`);
        }

        return matched.join('; ');
    }

    clearPartition(surfaceId, topLevelOrigin) {
        const key = `${surfaceId}:${topLevelOrigin}`;
        this._partitions.delete(key);
    }
}

/**
 * Anti-SSRF Node Network Broker & Single-Writer Cookie Synchronization Manager.
 */
class NetworkBroker {
    constructor(options = {}) {
        this.cookieStore = new HostCookieStore();
        this.allowLocalhost = options.allowLocalhost || false;
        this.maxResponseBodyBytes = options.maxResponseBodyBytes || MAX_RESPONSE_BODY_BYTES;
        this.requestTimeoutMs = options.requestTimeoutMs || REQUEST_TIMEOUT_MS;
        this.maxConcurrentRequests = options.maxConcurrentRequests || MAX_CONCURRENT_REQUESTS;
        this.maxRedirectHops = options.maxRedirectHops !== undefined ? options.maxRedirectHops : MAX_REDIRECT_HOPS;

        this._httpAgent = new http.Agent({ keepAlive: true, maxSockets: this.maxConcurrentRequests });
        this._httpsAgent = new https.Agent({ keepAlive: true, maxSockets: this.maxConcurrentRequests });

        this.activeRequestsCount = 0;
        this._concurrencyQueue = [];

        // Per-document synchronization states: `${surfaceId}:${documentId}` -> DocumentSyncState
        this._docStates = new Map();
    }

    async _acquireConcurrencySlot() {
        if (this.activeRequestsCount < this.maxConcurrentRequests) {
            this.activeRequestsCount++;
            return;
        }
        return new Promise((resolve) => {
            this._concurrencyQueue.push(resolve);
        });
    }

    _releaseConcurrencySlot() {
        if (this._concurrencyQueue.length > 0) {
            const next = this._concurrencyQueue.shift();
            next();
        } else {
            this.activeRequestsCount = Math.max(0, this.activeRequestsCount - 1);
        }
    }

    _getDocState(surfaceId, documentId) {
        const key = `${surfaceId}:${documentId}`;
        let state = this._docStates.get(key);
        if (!state) {
            state = {
                surfaceId,
                documentId,
                processedCookieSequence: 0,
                cookieSyncFaulted: false,
                pendingCookieMutations: new Map(), // seq_id -> { seq_id, cookieStr, attributes, isSizeValid, byteLength }
                pendingCookieBytes: 0,
                gapTimer: null,
                pendingFetchBarriers: [], // Array<{ requiredSequence, requestEnvelope, resolve, reject, byteLength, timer }>
                pendingFetchBytes: 0
            };
            this._docStates.set(key, state);
        }
        return state;
    }

    purgeDocumentSyncState(surfaceId, documentId) {
        const key = `${surfaceId}:${documentId}`;
        const state = this._docStates.get(key);
        if (state) {
            if (state.gapTimer) clearTimeout(state.gapTimer);
            for (const item of state.pendingFetchBarriers) {
                if (item.timer) clearTimeout(item.timer);
                item.reject(Object.assign(new Error('Document navigated or purged'), { code: 'E_BROKER_DISCONNECTED' }));
            }
            this._docStates.delete(key);
        }
    }

    /**
     * Faults the document's cookie synchronization state on overflow or gap expiration.
     */
    faultDocumentCookieSync(surface, documentId, reason) {
        const state = this._getDocState(surface.id, documentId);
        state.cookieSyncFaulted = true;
        surface.cookieSyncFaulted = true;

        if (state.gapTimer) {
            clearTimeout(state.gapTimer);
            state.gapTimer = null;
        }

        // Immediately fail all pending fetch barriers
        const barriers = state.pendingFetchBarriers;
        state.pendingFetchBarriers = [];
        state.pendingFetchBytes = 0;

        for (const item of barriers) {
            if (item.timer) clearTimeout(item.timer);
            item.reject(Object.assign(new Error(`Cookie sync faulted: ${reason}`), { code: reason }));
        }

        // Purge pending out-of-order mutations
        state.pendingCookieMutations.clear();
        state.pendingCookieBytes = 0;
    }

    /**
     * Enqueues an out-of-order cookie mutation into the document's reassembly buffer.
     */
    _enqueuePendingCookieMutationBounded(state, seq_id, cookieStr, attributes, isSizeValid) {
        if (state.pendingCookieMutations.size >= MAX_PENDING_COOKIE_MUTATIONS_PER_DOC) {
            return false;
        }

        const byteLength = Buffer.byteLength(cookieStr || '', 'utf8') + measureAttributesBytes(attributes);
        if (state.pendingCookieBytes + byteLength > MAX_PENDING_COOKIE_BYTES_PER_DOC) {
            return false;
        }

        state.pendingCookieMutations.set(seq_id, {
            seq_id,
            cookieStr,
            attributes,
            isSizeValid,
            byteLength
        });
        state.pendingCookieBytes += byteLength;

        // Start gap timer if not running
        if (!state.gapTimer) {
            state.gapTimer = setTimeout(() => {
                state.gapTimer = null;
                const surface = { id: state.surfaceId };
                this.faultDocumentCookieSync(surface, state.documentId, 'E_BROKER_COOKIE_GAP_EXPIRED');
            }, MAX_UNRESOLVED_COOKIE_GAP_MS);
            if (typeof state.gapTimer.unref === 'function') {
                state.gapTimer.unref();
            }
        }

        return true;
    }

    /**
     * Dequeues next pending cookie mutation from reassembly buffer.
     */
    _dequeuePendingCookieMutation(state, seq_id) {
        const item = state.pendingCookieMutations.get(seq_id);
        if (item) {
            state.pendingCookieMutations.delete(seq_id);
            state.pendingCookieBytes -= item.byteLength;
            return item;
        }
        return null;
    }

    /**
     * Handles an incoming cookie_set envelope from the engine.
     */
    onCookieSetEnvelope(surface, documentId, seq_id, cookieStr, attributes) {
        if (
            documentId !== surface.activeDocumentId ||
            surface.navigationState !== 'committed' ||
            surface.cookieSyncFaulted
        ) {
            return; // Drop stale or faulted document mutation
        }

        const state = this._getDocState(surface.id, documentId);
        if (state.cookieSyncFaulted) return;

        if (typeof surface.processedCookieSequence !== 'number') {
            surface.processedCookieSequence = 0;
        }

        if (seq_id <= state.processedCookieSequence) {
            return; // Ignore duplicate sequence
        }

        if (seq_id - state.processedCookieSequence > MAX_COOKIE_SEQ_GAP) {
            this.faultDocumentCookieSync(surface, documentId, 'E_BROKER_COOKIE_QUEUE_OVERFLOW');
            return;
        }

        const strBytes = Buffer.byteLength(cookieStr || '', 'utf8');
        const attrBytes = measureAttributesBytes(attributes);
        let isSizeValid =
            strBytes <= MAX_SINGLE_COOKIE_STRING_BYTES &&
            attrBytes <= MAX_SINGLE_COOKIE_ATTR_BYTES;

        // If out of order, buffer in reassembly queue
        if (seq_id !== state.processedCookieSequence + 1) {
            if (!this._enqueuePendingCookieMutationBounded(state, seq_id, cookieStr, attributes, isSizeValid)) {
                this.faultDocumentCookieSync(surface, documentId, 'E_BROKER_COOKIE_QUEUE_OVERFLOW');
            }
            return;
        }

        // Process contiguous mutations
        let currentItem = { seq_id, cookieStr, attributes, isSizeValid };
        while (currentItem) {
            const { seq_id: currSeq, cookieStr: currStr, isSizeValid: validSize } = currentItem;

            if (validSize) {
                const parsed = parseCookieString(currStr);
                // Script-set cookies cannot set HttpOnly
                if (parsed && !parsed.httpOnly) {
                    this.cookieStore.setCookie(surface.id, surface.currentOrigin, parsed);
                }
            }

            // Advance processed sequence for BOTH valid and rejected mutations
            state.processedCookieSequence = currSeq;
            surface.processedCookieSequence = currSeq;

            currentItem = this._dequeuePendingCookieMutation(state, state.processedCookieSequence + 1);
        }

        // Reset or clear gap timer
        if (state.pendingCookieMutations.size === 0) {
            if (state.gapTimer) {
                clearTimeout(state.gapTimer);
                state.gapTimer = null;
            }
        }

        this._drainEligibleFetchBarriers(surface, state);
    }

    /**
     * Drains all fetch barriers whose requiredCookieSequence is <= processedCookieSequence.
     */
    _drainEligibleFetchBarriers(surface, state) {
        if (state.pendingFetchBarriers.length === 0) return;

        const remaining = [];
        for (const item of state.pendingFetchBarriers) {
            if (state.processedCookieSequence >= item.requiredSequence) {
                if (item.timer) clearTimeout(item.timer);
                state.pendingFetchBytes -= item.byteLength;
                this._executeFetchWithRedirects(surface, item.requestEnvelope, item.resolve, item.reject);
            } else {
                remaining.push(item);
            }
        }
        state.pendingFetchBarriers = remaining;
    }

    /**
     * Handles broker_fetch_request from the engine.
     */
    handleBrokerFetchRequest(surface, documentId, requestEnvelope) {
        return new Promise((resolve, reject) => {
            const requiredSequence = requestEnvelope.requiredCookieSequence || 0;
            const state = this._getDocState(surface.id, documentId);

            if (surface.navigationState !== 'committed' || documentId !== surface.activeDocumentId) {
                return reject(Object.assign(new Error('Navigation not committed for document'), { code: 'E_BROKER_DISCONNECTED' }));
            }

            if (state.cookieSyncFaulted) {
                return reject(Object.assign(new Error('Cookie synchronization in faulted state'), { code: 'E_BROKER_COOKIE_SYNC_FAULT' }));
            }

            // Check if already satisfied
            if (state.processedCookieSequence >= requiredSequence) {
                return this._executeFetchWithRedirects(surface, requestEnvelope, resolve, reject);
            }

            // Check gap bound
            if (requiredSequence - state.processedCookieSequence > MAX_COOKIE_SEQ_GAP) {
                this.faultDocumentCookieSync(surface, documentId, 'E_BROKER_COOKIE_QUEUE_OVERFLOW');
                return reject(Object.assign(new Error('Cookie sequence gap exceeded limit'), { code: 'E_BROKER_COOKIE_QUEUE_OVERFLOW' }));
            }

            // Check barrier queue limits
            if (state.pendingFetchBarriers.length >= MAX_PENDING_FETCH_BARRIERS_PER_DOC) {
                this.faultDocumentCookieSync(surface, documentId, 'E_BROKER_COOKIE_QUEUE_OVERFLOW');
                return reject(Object.assign(new Error('Fetch barrier queue limit exceeded'), { code: 'E_BROKER_COOKIE_QUEUE_OVERFLOW' }));
            }

            const bodyBytes = requestEnvelope.body ? Buffer.byteLength(requestEnvelope.body) : 0;
            if (state.pendingFetchBytes + bodyBytes > MAX_PENDING_FETCH_BYTES_PER_DOC) {
                this.faultDocumentCookieSync(surface, documentId, 'E_BROKER_COOKIE_QUEUE_OVERFLOW');
                return reject(Object.assign(new Error('Fetch barrier byte limit exceeded'), { code: 'E_BROKER_COOKIE_QUEUE_OVERFLOW' }));
            }

            // Hold in barrier queue with bounded timeout
            const barrierItem = {
                requiredSequence,
                requestEnvelope,
                resolve,
                reject,
                byteLength: bodyBytes,
                timer: null
            };

            barrierItem.timer = setTimeout(() => {
                const idx = state.pendingFetchBarriers.indexOf(barrierItem);
                if (idx !== -1) {
                    state.pendingFetchBarriers.splice(idx, 1);
                    state.pendingFetchBytes -= barrierItem.byteLength;
                }
                reject(Object.assign(new Error('Fetch barrier timed out waiting for cookie sequence'), { code: 'E_BROKER_COOKIE_TIMEOUT' }));
            }, MAX_FETCH_BARRIER_TIMEOUT_MS);

            state.pendingFetchBarriers.push(barrierItem);
            state.pendingFetchBytes += bodyBytes;
        });
    }

    /**
     * Executes fetch with bounded redirects (up to 5 hops),
     * enforcing anti-SSRF validation at every hop and global concurrency caps.
     */
    async _executeFetchWithRedirects(surface, initialRequestEnvelope, resolve, reject) {
        await this._acquireConcurrencySlot();
        try {
            let currentEnvelope = { ...initialRequestEnvelope };
            let hopsRemaining = this.maxRedirectHops;

            while (true) {
                const hopResult = await this._performSingleHop(surface, currentEnvelope);

                const isRedirect = hopResult.status >= 300 && hopResult.status < 400 && hopResult.headers['location'];
                if (isRedirect) {
                    if (hopsRemaining <= 0) {
                        return reject(Object.assign(new Error('Redirect limit exceeded (max 5 hops)'), { code: 'E_BROKER_TOO_MANY_REDIRECTS' }));
                    }
                    hopsRemaining--;

                    const rawLocation = hopResult.headers['location'];
                    let nextUrl;
                    try {
                        nextUrl = new URL(rawLocation, currentEnvelope.url).toString();
                    } catch {
                        return reject(Object.assign(new Error(`Invalid redirect Location: ${rawLocation}`), { code: 'E_BROKER_INVALID_URL' }));
                    }

                    // RFC 7231 redirect method rewrites
                    let nextMethod = currentEnvelope.method || 'GET';
                    let nextBody = currentEnvelope.body;
                    if (hopResult.status === 303 || ((hopResult.status === 301 || hopResult.status === 302) && nextMethod === 'POST')) {
                        nextMethod = 'GET';
                        nextBody = undefined;
                    }

                    currentEnvelope = {
                        ...currentEnvelope,
                        url: nextUrl,
                        method: nextMethod,
                        body: nextBody
                    };
                    continue;
                }

                return resolve(hopResult);
            }
        } catch (err) {
            reject(err);
        } finally {
            this._releaseConcurrencySlot();
        }
    }

    /**
     * Performs a single network hop with IP pinning, anti-SSRF validation,
     * header hygiene, request timeout, and 64 MiB response body ceiling.
     */
    _performSingleHop(surface, requestEnvelope) {
        return new Promise(async (resolve, reject) => {
            const { url, method = 'GET', headers = {}, body } = requestEnvelope;

            let u;
            try {
                u = new URL(url);
            } catch {
                return reject(Object.assign(new Error('Invalid request URL'), { code: 'E_BROKER_INVALID_URL' }));
            }

            // Scheme check
            if (u.protocol !== 'http:' && u.protocol !== 'https:') {
                return reject(Object.assign(new Error(`Forbidden scheme: ${u.protocol}`), { code: 'E_BROKER_SCHEME_DENIED' }));
            }

            // Userinfo check (E_BROKER_USERINFO_FORBIDDEN)
            if (u.username || u.password) {
                return reject(Object.assign(new Error('Userinfo (username/password) in URL is forbidden'), { code: 'E_BROKER_USERINFO_FORBIDDEN' }));
            }

            // Destination origin validation against surface policy
            const destOrigin = normalizeOrigin(url);
            if (surface.trustedOrigins && !surface.trustedOrigins.has(destOrigin) && surface.trustedOrigins.size > 0) {
                return reject(Object.assign(new Error(`Destination origin ${destOrigin} denied by surface policy`), { code: 'E_BROKER_ORIGIN_DENIED' }));
            }

            // Resolve DNS and perform IP validation / pinning
            let resolvedIps;
            try {
                resolvedIps = await dns.promises.lookup(u.hostname, { all: true });
            } catch (err) {
                return reject(Object.assign(new Error(`DNS resolution failed for ${u.hostname}: ${err.message}`), { code: 'E_BROKER_DNS_FAILED' }));
            }

            if (!resolvedIps || resolvedIps.length === 0) {
                return reject(Object.assign(new Error(`No IP addresses found for ${u.hostname}`), { code: 'E_BROKER_DNS_FAILED' }));
            }

            // Verify every resolved IP
            for (const item of resolvedIps) {
                if (!isAllowedPublicIp(item.address, this.allowLocalhost)) {
                    return reject(Object.assign(new Error(`SSRF blocked: address ${item.address} is non-unicast or restricted`), { code: 'E_BROKER_SSRF_BLOCKED' }));
                }
            }

            // Pin first IP
            const pinned = resolvedIps[0];
            const isHttps = u.protocol === 'https:';
            const agent = isHttps ? this._httpsAgent : this._httpAgent;

            // Header hygiene: hop-by-hop stripping & connection nominated token stripping
            const cleanHeaders = {};
            let connectionTokens = [];
            for (const [k, v] of Object.entries(headers)) {
                const lk = k.toLowerCase();
                if (lk === 'connection' && typeof v === 'string') {
                    connectionTokens.push(...v.split(',').map(t => t.trim().toLowerCase()));
                }
            }

            for (const [k, v] of Object.entries(headers)) {
                const lk = k.toLowerCase();
                if (HOP_BY_HOP_HEADERS.has(lk)) continue;
                if (connectionTokens.includes(lk)) continue;
                cleanHeaders[k] = v;
            }

            // Attach host authoritative cookies
            const hostCookies = this.cookieStore.getCookieHeader(surface.id, surface.currentOrigin, url);
            if (hostCookies) {
                cleanHeaders['cookie'] = hostCookies;
            }

            // Host header
            cleanHeaders['host'] = u.host;

            const reqOptions = {
                method,
                agent,
                headers: cleanHeaders,
                lookup: (_hostname, _options, cb) => {
                    cb(null, pinned.address, pinned.family);
                }
            };

            if (isHttps) {
                reqOptions.servername = u.hostname; // Preserve TLS SNI
            }

            const transport = isHttps ? https : http;
            let settled = false;
            const finishReject = (err) => {
                if (!settled) {
                    settled = true;
                    reject(err);
                }
            };
            const finishResolve = (val) => {
                if (!settled) {
                    settled = true;
                    resolve(val);
                }
            };

            const req = transport.request(u, reqOptions, (res) => {
                const chunks = [];
                let totalBytes = 0;

                res.on('data', (c) => {
                    totalBytes += c.length;
                    if (totalBytes > this.maxResponseBodyBytes) {
                        req.destroy();
                        finishReject(Object.assign(new Error('Response body exceeded maximum limit of 64 MiB'), { code: 'E_BROKER_BODY_TOO_LARGE' }));
                        return;
                    }
                    chunks.push(c);
                });

                res.on('end', () => {
                    const responseBuffer = Buffer.concat(chunks);

                    // Process Set-Cookie headers from response
                    const setCookieHeaders = res.headers['set-cookie'];
                    const clientSetCookies = [];
                    if (setCookieHeaders) {
                        const cookieList = Array.isArray(setCookieHeaders) ? setCookieHeaders : [setCookieHeaders];
                        for (const raw of cookieList) {
                            const parsed = parseCookieString(raw);
                            if (parsed) {
                                this.cookieStore.setCookie(surface.id, surface.currentOrigin, parsed);
                                if (!parsed.httpOnly) {
                                    clientSetCookies.push(parsed);
                                }
                            }
                        }
                    }

                    finishResolve({
                        status: res.statusCode,
                        statusText: res.statusMessage,
                        headers: res.headers,
                        body: responseBuffer,
                        clientSetCookies // Non-HttpOnly cookies for Servo local DOM synchronization
                    });
                });
            });

            // Request timeout
            req.setTimeout(this.requestTimeoutMs, () => {
                req.destroy(Object.assign(new Error('Broker request timed out'), { code: 'E_BROKER_TIMEOUT' }));
            });

            req.on('error', (err) => {
                finishReject(Object.assign(new Error(`Network request failed: ${err.message}`), { code: err.code || 'E_BROKER_NETWORK_ERROR' }));
            });

            if (body) {
                req.write(body);
            }
            req.end();
        });
    }
}

module.exports = {
    NetworkBroker,
    HostCookieStore,
    isAllowedPublicIp,
    normalizeOrigin,
    parseCookieString,
    measureAttributesBytes,
    MAX_COOKIE_SEQ_GAP,
    MAX_PENDING_COOKIE_MUTATIONS_PER_DOC,
    MAX_PENDING_COOKIE_BYTES_PER_DOC,
    MAX_SINGLE_COOKIE_STRING_BYTES,
    MAX_SINGLE_COOKIE_ATTR_BYTES,
    MAX_PENDING_FETCH_BARRIERS_PER_DOC,
    MAX_PENDING_FETCH_BYTES_PER_DOC,
    MAX_UNRESOLVED_COOKIE_GAP_MS,
    MAX_FETCH_BARRIER_TIMEOUT_MS,
    MAX_RESPONSE_BODY_BYTES,
    MAX_CONCURRENT_REQUESTS,
    REQUEST_TIMEOUT_MS,
    MAX_REDIRECT_HOPS
};
