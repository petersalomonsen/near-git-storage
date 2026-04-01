/**
 * NEAR Git Service Worker
 *
 * Intercepts git smart HTTP requests and translates them to NEAR contract
 * calls, bypassing any external git HTTP server.
 *
 * Configuration is received via a 'configure' message from the main thread:
 *   { nearRpcUrl, contractId }
 */

let config = null;
let configPromise = null;

self.addEventListener('message', (event) => {
    if (event.data && event.data.type === 'configure') {
        config = event.data;
        console.log('[near-git-sw] configured via message:', config);
    }
});

async function ensureConfig() {
    if (config) return config;
    if (configPromise) return configPromise;
    configPromise = (async () => {
        // Get config via same-origin proxy (avoids COEP issues)
        const res = await fetch(`${self.location.origin}/near-info`);
        const nearInfo = await res.json();
        config = {
            nearRpcUrl: `${self.location.origin}/near-rpc`,
            nearCallUrl: `${self.location.origin}/near-call`,
            contractId: nearInfo.contractId,
        };
        console.log('[near-git-sw] auto-configured:', config);
        return config;
    })();
    return configPromise;
}

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => {
    event.waitUntil(self.clients.claim());
});

self.addEventListener('fetch', (event) => {
    const url = new URL(event.request.url);

    // Only intercept git protocol requests under /near-repo/
    if (!url.pathname.startsWith('/near-repo/')) {
        return;
    }

    event.respondWith(
        handleGitRequest(event.request, url).catch(err => {
            console.error('[near-git-sw] error handling', url.pathname, err);
            return new Response('Service worker error: ' + err.message, { status: 500 });
        })
    );
});

async function handleGitRequest(request, url) {
    await ensureConfig();

    const path = url.pathname;

    // GET /repo/info/refs?service=...
    if (path.endsWith('/info/refs')) {
        const service = url.searchParams.get('service');
        return handleInfoRefs(service);
    }

    // POST /repo/git-receive-pack
    if (path.endsWith('/git-receive-pack')) {
        const body = await request.arrayBuffer();
        return handleReceivePack(new Uint8Array(body));
    }

    // POST /repo/git-upload-pack
    if (path.endsWith('/git-upload-pack')) {
        const body = await request.arrayBuffer();
        return handleUploadPack(new Uint8Array(body));
    }

    return new Response('Not found', { status: 404 });
}

// --- NEAR RPC helpers ---

async function nearViewCall(method, args) {
    const res = await fetch(config.nearRpcUrl, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            jsonrpc: '2.0',
            id: 'q',
            method: 'query',
            params: {
                request_type: 'call_function',
                finality: 'optimistic',
                account_id: config.contractId,
                method_name: method,
                args_base64: btoa(JSON.stringify(args)),
            },
        }),
    });
    const json = await res.json();
    if (json.error) throw new Error(JSON.stringify(json.error));
    const resultBytes = new Uint8Array(json.result.result);
    return JSON.parse(new TextDecoder().decode(resultBytes));
}

// --- pkt-line helpers ---

function pktLineEncode(data) {
    const len = data.length + 4;
    const hex = len.toString(16).padStart(4, '0');
    const encoder = new TextEncoder();
    const prefix = encoder.encode(hex);
    const result = new Uint8Array(prefix.length + data.length);
    result.set(prefix);
    result.set(data, prefix.length);
    return result;
}

function pktLineFlush() {
    return new TextEncoder().encode('0000');
}

function pktLineEncodeString(str) {
    return pktLineEncode(new TextEncoder().encode(str));
}

function readPktLines(data) {
    const lines = [];
    let pos = 0;
    const decoder = new TextDecoder();

    while (pos + 4 <= data.length) {
        const lenHex = decoder.decode(data.slice(pos, pos + 4));
        const len = parseInt(lenHex, 16);
        if (len === 0) { pos += 4; break; }
        if (len < 4) break;
        lines.push(data.slice(pos + 4, pos + len));
        pos += len;
    }
    return { lines, rest: data.slice(pos) };
}

// --- Packfile helpers ---

function zlibDecompress(data) {
    const ds = new DecompressionStream('deflate');
    const writer = ds.writable.getWriter();
    writer.write(data);
    writer.close();
    return new Response(ds.readable).arrayBuffer().then(buf => new Uint8Array(buf));
}

function zlibCompress(data) {
    const cs = new CompressionStream('deflate');
    const writer = cs.writable.getWriter();
    writer.write(data);
    writer.close();
    return new Response(cs.readable).arrayBuffer().then(buf => new Uint8Array(buf));
}

const OBJ_TYPES = { 1: 'commit', 2: 'tree', 3: 'blob', 4: 'tag' };
const OBJ_TYPE_BITS = { commit: 1, tree: 2, blob: 3, tag: 4 };

async function parsePackfile(data) {
    if (data.length < 12) throw new Error('packfile too short');
    const magic = new TextDecoder().decode(data.slice(0, 4));
    if (magic !== 'PACK') throw new Error('bad packfile magic');
    const numObjects = (data[8] << 24) | (data[9] << 16) | (data[10] << 8) | data[11];

    let pos = 12;
    const objects = [];
    const deltas = [];

    for (let i = 0; i < numObjects; i++) {
        let byte = data[pos];
        const typeBits = (byte >> 4) & 0x07;
        let size = byte & 0x0f;
        let shift = 4;
        pos++;

        while (byte & 0x80) {
            byte = data[pos++];
            size |= (byte & 0x7f) << shift;
            shift += 7;
        }

        if (typeBits === 7) {
            // REF_DELTA
            const baseSha = Array.from(data.slice(pos, pos + 20))
                .map(b => b.toString(16).padStart(2, '0')).join('');
            pos += 20;
            // Find end of zlib data by decompressing
            const remaining = data.slice(pos);
            const deltaData = await zlibDecompress(remaining);
            // Approximate consumed bytes - decompress eats the right amount
            // Use a heuristic: try increasing sizes until decompress works
            let consumed = findZlibEnd(remaining, deltaData.length);
            pos += consumed;
            deltas.push({ baseSha, deltaData });
        } else if (typeBits === 6) {
            // OFS_DELTA
            let ob = data[pos++];
            let offset = ob & 0x7f;
            while (ob & 0x80) {
                ob = data[pos++];
                offset = ((offset + 1) << 7) | (ob & 0x7f);
            }
            const remaining = data.slice(pos);
            const deltaData = await zlibDecompress(remaining);
            let consumed = findZlibEnd(remaining, deltaData.length);
            pos += consumed;
            // Can't resolve ofs_delta in SW easily, skip
            throw new Error('ofs_delta not supported in service worker');
        } else {
            const typeName = OBJ_TYPES[typeBits];
            if (!typeName) throw new Error('unknown type: ' + typeBits);
            const remaining = data.slice(pos);
            const objData = await zlibDecompress(remaining);
            let consumed = findZlibEnd(remaining, objData.length);
            pos += consumed;
            objects.push({ obj_type: typeName, data: objData });
        }
    }

    return { objects, deltas };
}

function findZlibEnd(compressed, expectedSize) {
    // The zlib stream in a packfile is followed by the next object.
    // We can't easily know where it ends without a proper zlib parser.
    // Heuristic: zlib overhead is roughly 2-12 bytes + deflate ratio.
    // Try decompressing with increasing windows until we get the right output.
    // Simpler approach: zlib header is 2 bytes, deflate blocks follow.
    // For now, scan for the next valid packfile object header or checksum.

    // Actually, use a simple approach: try decompressing the whole remaining buffer.
    // The DecompressionStream will stop reading at the end of the zlib stream.
    // Since we can't get consumed bytes from DecompressionStream, estimate:
    // compressed size is roughly expectedSize * 0.6 to 1.5
    // Try a safe upper bound and binary search... too complex.

    // Practical: zlib streams in packfiles are individually framed.
    // The compressed data is typically smaller than the uncompressed.
    // Just use a generous estimate.
    return Math.min(compressed.length, Math.max(expectedSize + 20, Math.ceil(expectedSize * 1.5)));
}

async function buildPackfile(objects) {
    const parts = [];

    // Header
    const header = new Uint8Array(12);
    header.set(new TextEncoder().encode('PACK'));
    header[4] = 0; header[5] = 0; header[6] = 0; header[7] = 2; // version 2
    const n = objects.length;
    header[8] = (n >> 24) & 0xff;
    header[9] = (n >> 16) & 0xff;
    header[10] = (n >> 8) & 0xff;
    header[11] = n & 0xff;
    parts.push(header);

    for (const obj of objects) {
        const typeBits = OBJ_TYPE_BITS[obj.obj_type];
        const size = obj.data.length;

        // Encode type + size
        const sizeBytes = [];
        let firstByte = (typeBits << 4) | (size & 0x0f);
        let remaining = size >> 4;
        if (remaining > 0) firstByte |= 0x80;
        sizeBytes.push(firstByte);
        while (remaining > 0) {
            let b = remaining & 0x7f;
            remaining >>= 7;
            if (remaining > 0) b |= 0x80;
            sizeBytes.push(b);
        }
        parts.push(new Uint8Array(sizeBytes));

        // Compress
        const compressed = await zlibCompress(obj.data);
        parts.push(compressed);
    }

    // Combine
    let totalLen = parts.reduce((s, p) => s + p.length, 0);
    const result = new Uint8Array(totalLen + 20); // + SHA-1 checksum
    let offset = 0;
    for (const p of parts) {
        result.set(p, offset);
        offset += p.length;
    }

    // SHA-1 checksum over all preceding data
    const hashBuf = await crypto.subtle.digest('SHA-1', result.slice(0, offset));
    result.set(new Uint8Array(hashBuf), offset);

    return result;
}

function applyDelta(source, delta) {
    let pos = 0;
    // Read source size
    let sourceSize = 0, shift = 0, byte;
    do { byte = delta[pos++]; sourceSize |= (byte & 0x7f) << shift; shift += 7; } while (byte & 0x80);
    // Read target size
    let targetSize = 0; shift = 0;
    do { byte = delta[pos++]; targetSize |= (byte & 0x7f) << shift; shift += 7; } while (byte & 0x80);

    const target = new Uint8Array(targetSize);
    let tpos = 0;

    while (pos < delta.length) {
        const inst = delta[pos++];
        if (inst & 0x80) {
            let offset = 0, size = 0;
            if (inst & 0x01) offset |= delta[pos++];
            if (inst & 0x02) offset |= delta[pos++] << 8;
            if (inst & 0x04) offset |= delta[pos++] << 16;
            if (inst & 0x08) offset |= delta[pos++] << 24;
            if (inst & 0x10) size |= delta[pos++];
            if (inst & 0x20) size |= delta[pos++] << 8;
            if (inst & 0x40) size |= delta[pos++] << 16;
            if (size === 0) size = 0x10000;
            target.set(source.slice(offset, offset + size), tpos);
            tpos += size;
        } else if (inst !== 0) {
            target.set(delta.slice(pos, pos + inst), tpos);
            tpos += inst;
            pos += inst;
        }
    }
    return target;
}

async function gitSha1(objType, data) {
    const header = new TextEncoder().encode(`${objType} ${data.length}\0`);
    const buf = new Uint8Array(header.length + data.length);
    buf.set(header);
    buf.set(data, header.length);
    const hash = await crypto.subtle.digest('SHA-1', buf);
    return Array.from(new Uint8Array(hash)).map(b => b.toString(16).padStart(2, '0')).join('');
}

// --- Git protocol handlers ---

const ZERO_SHA = '0000000000000000000000000000000000000000';
const CAPABILITIES = 'report-status delete-refs';

async function handleInfoRefs(service) {
    const refs = await nearViewCall('get_refs', {});

    const parts = [];
    parts.push(pktLineEncodeString(`# service=${service}\n`));
    parts.push(pktLineFlush());

    if (refs.length === 0) {
        parts.push(pktLineEncodeString(`${ZERO_SHA} capabilities^{}\0${CAPABILITIES}\n`));
    } else {
        // Determine HEAD: prefer refs/heads/main, then refs/heads/master, then first ref
        const headEntry = refs.find(([name]) => name === 'refs/heads/main')
            || refs.find(([name]) => name === 'refs/heads/master')
            || refs[0];
        const [headRef, headSha] = headEntry;
        const caps = `${CAPABILITIES} symref=HEAD:${headRef}`;

        // HEAD must be first ref advertised (carries capabilities)
        parts.push(pktLineEncodeString(`${headSha} HEAD\0${caps}\n`));
        for (const [refName, sha] of refs) {
            parts.push(pktLineEncodeString(`${sha} ${refName}\n`));
        }
    }
    parts.push(pktLineFlush());

    const body = concatUint8Arrays(parts);
    return new Response(body, {
        headers: { 'Content-Type': `application/x-${service}-advertisement` },
    });
}

async function handleReceivePack(body) {
    const { lines, rest } = readPktLines(body);
    const decoder = new TextDecoder();

    const refUpdates = [];
    for (const line of lines) {
        const text = decoder.decode(line).trim();
        const parts = text.split(' ');
        if (parts.length >= 3) {
            const oldSha = parts[0].split('\0').pop();
            const newSha = parts[1];
            const refName = parts.slice(2).join(' ').split('\0')[0];
            refUpdates.push({
                name: refName,
                old_sha: oldSha === ZERO_SHA ? null : oldSha,
                new_sha: newSha,
            });
        }
    }

    if (rest.length > 0) {
        // Parse packfile using server-side parser (avoids zlib boundary issues in browser)
        const parseRes = await fetch(`${self.location.origin}/parse-packfile`, {
            method: 'POST',
            body: rest,
        });
        const parseResult = await parseRes.json();
        if (parseResult.error) {
            return makeReceivePackResponse([`ng unpack ${parseResult.error}`]);
        }

        // Convert base64 data back to Uint8Array for delta resolution
        let allObjects = parseResult.objects.map(obj => ({
            obj_type: obj.obj_type,
            data: Uint8Array.from(atob(obj.data), c => c.charCodeAt(0)),
        }));

        // Resolve deltas
        if (parseResult.deltas && parseResult.deltas.length > 0) {
            const localMap = {};
            for (const obj of allObjects) {
                const sha = await gitSha1(obj.obj_type, obj.data);
                localMap[sha] = obj;
            }

            for (const delta of parseResult.deltas) {
                const deltaData = Uint8Array.from(atob(delta.delta_data), c => c.charCodeAt(0));
                let base = localMap[delta.base_sha];
                if (!base) {
                    const result = await nearViewCall('get_objects', { shas: [delta.base_sha] });
                    if (result[0] && result[0][1]) {
                        const objData = result[0][1];
                        base = {
                            obj_type: objData.obj_type,
                            data: Uint8Array.from(atob(objData.data), c => c.charCodeAt(0)),
                        };
                    }
                }
                if (!base) {
                    return makeReceivePackResponse([`ng unpack base ${delta.base_sha} not found`]);
                }

                const resolved = applyDelta(base.data, deltaData);
                const obj = { obj_type: base.obj_type, data: resolved };
                const sha = await gitSha1(obj.obj_type, obj.data);
                localMap[sha] = obj;
                allObjects.push(obj);
            }
        }

        // Push objects to contract
        const gitObjects = allObjects.map(obj => ({
            obj_type: obj.obj_type,
            data: uint8ArrayToBase64(obj.data),
        }));

        const pushResult = await nearFunctionCall('push_objects', { objects: gitObjects });
        if (!pushResult.success) {
            return makeReceivePackResponse([`ng unpack ${pushResult.error}`]);
        }

        const objectShas = pushResult.result.shas;
        const txHash = pushResult.txHash;

        // Register push
        const registerResult = await nearFunctionCall('register_push', {
            tx_hash: txHash,
            object_shas: objectShas,
            ref_updates: refUpdates,
        });
        if (!registerResult.success) {
            return makeReceivePackResponse([`ng refs ${registerResult.error}`]);
        }
    }

    const statusLines = ['unpack ok'];
    for (const update of refUpdates) {
        statusLines.push(`ok ${update.name}`);
    }
    return makeReceivePackResponse(statusLines);
}

async function nearFunctionCall(method, args) {
    // Use the git-server's /near-call endpoint which handles signing
    try {
        const res = await fetch(config.nearCallUrl, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ method, args }),
        });
        const json = await res.json();
        return json;
    } catch (e) {
        return { success: false, error: e.message };
    }
}

async function handleUploadPack(body) {
    const { lines } = readPktLines(body);
    const decoder = new TextDecoder();
    const wants = [];
    const haves = [];

    for (const line of lines) {
        const text = decoder.decode(line).trim();
        if (text.startsWith('want ')) wants.push(text.split(' ')[1]);
        else if (text.startsWith('have ')) haves.push(text.split(' ')[1]);
    }

    if (wants.length === 0) {
        const nak = pktLineEncodeString('NAK\n');
        return new Response(concatUint8Arrays([nak, pktLineFlush()]), {
            headers: { 'Content-Type': 'application/x-git-upload-pack-result' },
        });
    }

    // Walk object graph
    const havesSet = new Set(haves);
    const needed = [];
    const visited = new Set();
    const queue = [...wants];

    while (queue.length > 0) {
        const sha = queue.shift();
        if (visited.has(sha) || havesSet.has(sha)) continue;
        visited.add(sha);
        needed.push(sha);

        const objects = await nearViewCall('get_objects', { shas: [sha] });
        for (const [, maybeObj] of objects) {
            if (!maybeObj) continue;
            const data = Uint8Array.from(atob(maybeObj.data), c => c.charCodeAt(0));

            if (maybeObj.obj_type === 'commit') {
                const text = decoder.decode(data);
                for (const line of text.split('\n')) {
                    if (line.startsWith('tree ')) queue.push(line.slice(5).trim());
                    else if (line.startsWith('parent ')) queue.push(line.slice(7).trim());
                    else if (line === '') break;
                }
            } else if (maybeObj.obj_type === 'tree') {
                let pos = 0;
                while (pos < data.length) {
                    const nullPos = data.indexOf(0, pos);
                    if (nullPos === -1 || nullPos + 21 > data.length) break;
                    const childSha = Array.from(data.slice(nullPos + 1, nullPos + 21))
                        .map(b => b.toString(16).padStart(2, '0')).join('');
                    queue.push(childSha);
                    pos = nullPos + 21;
                }
            }
        }
    }

    // Fetch all needed objects
    const packObjects = [];
    for (let i = 0; i < needed.length; i += 50) {
        const chunk = needed.slice(i, i + 50);
        const objects = await nearViewCall('get_objects', { shas: chunk });
        for (const [, maybeObj] of objects) {
            if (maybeObj) {
                packObjects.push({
                    obj_type: maybeObj.obj_type,
                    data: Uint8Array.from(atob(maybeObj.data), c => c.charCodeAt(0)),
                });
            }
        }
    }

    const packData = await buildPackfile(packObjects);

    const nak = pktLineEncodeString('NAK\n');
    const responseBody = concatUint8Arrays([nak, packData]);
    return new Response(responseBody, {
        headers: { 'Content-Type': 'application/x-git-upload-pack-result' },
    });
}

function makeReceivePackResponse(lines) {
    const parts = lines.map(l => pktLineEncodeString(l + '\n'));
    parts.push(pktLineFlush());
    return new Response(concatUint8Arrays(parts), {
        headers: { 'Content-Type': 'application/x-git-receive-pack-result' },
    });
}

function concatUint8Arrays(arrays) {
    const total = arrays.reduce((s, a) => s + a.length, 0);
    const result = new Uint8Array(total);
    let offset = 0;
    for (const a of arrays) { result.set(a, offset); offset += a.length; }
    return result;
}

function uint8ArrayToBase64(data) {
    let binary = '';
    for (let i = 0; i < data.length; i++) binary += String.fromCharCode(data[i]);
    return btoa(binary);
}
