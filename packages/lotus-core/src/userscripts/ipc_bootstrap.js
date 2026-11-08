window.lotus = {
    handlers: {},
    _ws: null,
    _offlineQueue: [],
    _batch: [],
    _batchBytes: 0,
    _batchHasControl: false,
    _batchTimeout: null,
    _pendingInvokes: {},
    port: null, // Will be set by init script
    token: null, // Will be set by init script
    id: null,    // Will be set by init script
    _assemblies: {},
    _packer: null,
    _unpacker: null,
    _getPacker: () => {
        if (!window.lotus._packer && window.msgpackr) {
            window.lotus._packer = new window.msgpackr.Packr({ useRecords: false });
        }
        return window.lotus._packer;
    },
    _getUnpacker: () => {
        if (!window.lotus._unpacker && window.msgpackr) {
            window.lotus._unpacker = new window.msgpackr.Unpackr({ useRecords: false });
        }
        return window.lotus._unpacker;
    },
    _handleDecoded: (decodedMsgs) => {
        // Single message unwrapping to match standard IPC emission payload
        if (!Array.isArray(decodedMsgs) || decodedMsgs.length === 0) return;
        
        // Handle Batch format or Array
        // If batch [[channel, payload], [channel, payload]]
        if (Array.isArray(decodedMsgs[0])) {
            for (const [channel, payload] of decodedMsgs) {
                window.lotus.emit(channel, payload);
            }
        } else if (decodedMsgs.length >= 2 && typeof decodedMsgs[0] === 'string') {
            // Single message [channel, payload]
            const channel = decodedMsgs[0];
            const payload = decodedMsgs[1]; // Payload might be undefined if 0 args
            window.lotus.emit(channel, payload);
        }
    },
    _processRaw: (data) => {
        if (!window.msgpackr) return;
        const unpacker = window.lotus._getUnpacker();
        if (!unpacker) return;

        const type = data[0];
        if (type === 0x02) {
            try {
                const decodedMsgs = unpacker.unpack(data.subarray(1));
                window.lotus._handleDecoded(decodedMsgs);
            } catch (e) {
                console.error("Lotus IPC unpack error", e);
            }
        } else if (type === 0x03) {
            window.lotus._handleChunk(data);
        }
    },
    _handleChunk: (data) => {
        if (data.length < 9) return;
        // Protocol: [Type(1)][MsgID(4)][Total(2)][Index(2)][Payload(N)]
        const dv = new DataView(data.buffer, data.byteOffset, data.byteLength);
        const msgId = dv.getUint32(1);
        const total = dv.getUint16(5);
        const index = dv.getUint16(7);
        const payload = data.subarray(9);
        
        if (!window.lotus._assemblies[msgId]) {
            window.lotus._assemblies[msgId] = {
                chunks: new Array(total),
                received: 0,
                lastActivity: Date.now()
            };
        }
        
        const assembly = window.lotus._assemblies[msgId];
        if (!assembly.chunks[index]) {
            assembly.chunks[index] = payload;
            assembly.received++;
            assembly.lastActivity = Date.now();
        }
        
        if (assembly.received === total) {
            delete window.lotus._assemblies[msgId];
            const totalLen = assembly.chunks.reduce((acc, c) => acc + c.length, 0);
            const full = new Uint8Array(totalLen);
            let offset = 0;
            for (const c of assembly.chunks) {
                full.set(c, offset);
                offset += c.length;
            }
            try {
                const unpacker = window.lotus._getUnpacker();
                if (unpacker) {
                    const decodedMsgs = unpacker.unpack(full);
                    window.lotus._handleDecoded(decodedMsgs);
                }
            } catch (e) {
                console.error("Lotus IPC reassembly unpack error", e);
            }
        }
    },

    _connectWs: () => {
       if (window.lotus._ws || !window.lotus.port) return;

       const wsUrl = `ws://127.0.0.1:${window.lotus.port}/ws?token=${window.lotus.token}&id=${window.lotus.id}&paneId=${window.lotus.paneId}`;
       window.lotus._ws = new WebSocket(wsUrl);        window.lotus._ws.binaryType = 'arraybuffer';
        
        window.lotus._ws.onopen = () => {
            // console.log("Lotus IPC WebSocket connected");
            const queue = window.lotus._offlineQueue;
            window.lotus._offlineQueue = [];
            for (const msg of queue) {
                window.lotus._ws.send(msg);
            }
        };

        window.lotus._ws.onclose = () => {
            // console.warn("Lotus IPC WebSocket disconnected, reconnecting in 1s...");
            window.lotus._ws = null;
            setTimeout(window.lotus._connectWs, 1000);
        };

        window.lotus._ws.onerror = (e) => {
            // console.error("Lotus IPC WebSocket error", e);
        };

        window.lotus._ws.onmessage = async (event) => {
            try {
                let data = event.data;
                // If it's a blob, we must await its arrayBuffer to unpack it
                if (data instanceof Blob) {
                    data = await data.arrayBuffer();
                }
                
                if (data instanceof ArrayBuffer) {
                    window.lotus._processRaw(new Uint8Array(data));
                }
            } catch (e) {
                console.error("Lotus IPC message handling error", e);
            }
        };
    },

    send: (channel, data, options = {}) => {
        if (!window.lotus.port) {
            console.error("Lotus IPC port not initialized");
            return;
        }

        // Initialize connection lazily on first send, or explicitly elsewhere
        if (!window.lotus._ws && channel !== "lotus:internal-reconnect") {
            window.lotus._connectWs();
        }

        // All payloads — text, JSON, and binary (Blob/ArrayBuffer/TypedArray) —
        // enter the same batch queue. msgpackr encodes binary entries as msgpack
        // bin naturally, so no special-casing is needed here.
        window.lotus._batch.push([channel, data]);
        
        // SAFE CHECK: Ensure data is defined before accessing byteLength
        const dataLen = data ? (typeof data === 'string' ? data.length * 2 : (data.byteLength || 0)) : 0;
        window.lotus._batchBytes += dataLen;

        if (channel === 'lotus:set-drag-regions') {
            window.lotus._batchHasControl = true;
        }

        const flushBatch = () => {
            if (window.lotus._batch.length === 0) return;
            const batchToFlush = window.lotus._batch;
            const hasControl = window.lotus._batchHasControl;
            window.lotus._batch = [];
            window.lotus._batchBytes = 0;
            window.lotus._batchHasControl = false;
            window.lotus._batchTimeout = null;

            if (window.msgpackr) {
                const packer = window.lotus._getPacker();
                if (!packer) return;
                try {
                    const packed = packer.pack(batchToFlush);
                    const CHUNK_THRESHOLD = 1024 * 1024; // 1MB
                    if (packed.length > CHUNK_THRESHOLD) {
                        window.lotus._chunkAndSend(packed, hasControl);
                    } else {
                        const finalPayload = new Uint8Array(packed.length + 1);
                        finalPayload[0] = hasControl ? 0x01 : 0x02;
                        finalPayload.set(packed, 1);
                        window.lotus._rawSend(finalPayload);
                    }
                } catch (e) {
                    console.error("Failed to pack batch", e);
                }
            } else {
                console.error("msgpackr not loaded");
            }
        };

        window.lotus._chunkAndSend = (packed, hasControl) => {
            const CHUNK_SIZE = 128 * 1024;
            const total = Math.ceil(packed.length / CHUNK_SIZE);
            const msgId = (Math.random() * 0xFFFFFFFF) >>> 0;
            
            let index = 0;
            const sendNext = () => {
                if (index >= total) return;
                
                const start = index * CHUNK_SIZE;
                const end = Math.min(start + CHUNK_SIZE, packed.length);
                const chunkPayload = packed.subarray(start, end);
                
                const final = new Uint8Array(chunkPayload.length + 9);
                final[0] = 0x03;
                const dv = new DataView(final.buffer);
                dv.setUint32(1, msgId);
                dv.setUint16(5, total);
                dv.setUint16(7, index);
                final.set(chunkPayload, 9);
                
                window.lotus._rawSend(final);
                index++;
                if (index < total) {
                    // Use setTimeout in the Renderer to ensure the event loop yields
                    // to rendering/composition between chunks.
                    setTimeout(sendNext, 0);
                }
            };
            sendNext();
        };

        window.lotus._rawSend = (data) => {
            if (window.lotus._ws && window.lotus._ws.readyState === WebSocket.OPEN) {
                window.lotus._ws.send(data);
            } else {
                window.lotus._offlineQueue.push(data);
            }
        };

        // Eager flush to pipeline large bursts instead of packing 100 MB at once
        if (options.urgent || window.lotus._batch.length >= 800 || window.lotus._batchBytes >= 1024 * 1024) {
            flushBatch();
        } else if (!window.lotus._batchTimeout) {
            queueMicrotask(flushBatch);
            window.lotus._batchTimeout = true;
        }
    },
    invoke: (channel, data) => {
        return new Promise((resolve, reject) => {
            const replyId = 'lotus:reply:' + Math.random().toString(36).slice(2) + Date.now().toString(36);
            const timeoutMs = 30000;
            const timer = setTimeout(() => {
                delete window.lotus._pendingInvokes[replyId];
                reject(new Error(`lotus.invoke('${channel}') timed out after ${timeoutMs}ms`));
            }, timeoutMs);
            window.lotus._pendingInvokes[replyId] = (result) => {
                clearTimeout(timer);
                delete window.lotus._pendingInvokes[replyId];
                if (result && result._error !== undefined) {
                    reject(new Error(result._error));
                } else {
                    resolve(result);
                }
            };
            // Guard: if data is null/undefined/primitive, spread nothing rather than throwing.
            // Objects are spread normally so all existing keys are preserved.
            const payload = Object.assign(
                {},
                (data !== null && typeof data === 'object') ? data : {},
                { _replyId: replyId }
            );
            window.lotus.send(channel, payload);
        });
    },
    on: (channel, handler) => {
        if (!window.lotus.handlers[channel]) window.lotus.handlers[channel] = [];
        window.lotus.handlers[channel].push(handler);
    },
    off: (channel, handler) => {
        if (window.lotus.handlers[channel]) {
            const index = window.lotus.handlers[channel].indexOf(handler);
            if (index !== -1) window.lotus.handlers[channel].splice(index, 1);
        }
    },
    emit: (channel, data) => {
        // Route replies to pending invoke() calls first
        if (channel.startsWith('lotus:reply:')) {
            const cb = window.lotus._pendingInvokes[channel];
            if (cb) { cb(data); }
            return;
        }
        (window.lotus.handlers[channel] || []).forEach(h => h(data));
    }
};
