/**
 * NEAR Git Service Worker (standalone)
 *
 * Intercepts git smart HTTP requests and translates them to NEAR contract
 * calls — no git HTTP server required. Uses a WASM module for packfile
 * parsing/building and NEAR transaction signing.
 *
 * Configuration is received via a 'configure' message from the main thread:
 *   { type: 'configure', rpcUrl, contractId, accountId, publicKey, privateKey }
 */

import init, {
    parse_packfile,
    build_packfile,
    apply_delta,
    git_sha1,
    create_signed_transaction,
} from './wasm-lib/wasm_lib.js';

let config = null;
let configPromise = null;
let wasmReady = false;

const wasmInit = init(new URL('./wasm-lib/wasm_lib_bg.wasm', self.location.origin));

self.addEventListener('message', (event) => {
    if (event.data && event.data.type === 'configure') {
        config = event.data;
        console.log('[near-git-sw] configured:', config.contractId, config.accountId);
    }
});

async function ensureReady() {
    if (!wasmReady) {
        await wasmInit;
        wasmReady = true;
    }
    if (!config) {
        if (!configPromise) {
            configPromise = new Promise(resolve => {
                const handler = (event) => {
                    if (event.data && event.data.type === 'configure') {
                        config = event.data;
                        self.removeEventListener('message', handler);
                        resolve(config);
                    }
                };
                self.addEventListener('message', handler);
            });
        }
        await configPromise;
    }
}

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => {
    event.waitUntil(self.clients.claim());
});

self.addEventListener('fetch', (event) => {
    const url = new URL(event.request.url);
    if (!url.pathname.startsWith('/near-repo/')) return;

    event.respondWith(
        handleGitRequest(event.request, url).catch(err => {
            console.error('[near-git-sw] error handling', url.pathname, err);
            return new Response('Service worker error: ' + err.message, { status: 500 });
        })
    );
});

async function handleGitRequest(request, url) {
    await ensureReady();
    const path = url.pathname;

    if (path.endsWith('/info/refs')) {
        return handleInfoRefs(url.searchParams.get('service'));
    }
    if (path.endsWith('/git-receive-pack')) {
        return handleReceivePack(new Uint8Array(await request.arrayBuffer()));
    }
    if (path.endsWith('/git-upload-pack')) {
        return handleUploadPack(new Uint8Array(await request.arrayBuffer()));
    }
    return new Response('Not found', { status: 404 });
}

// --- NEAR RPC ---

async function nearRpc(method, params) {
    const res = await fetch(config.rpcUrl, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ jsonrpc: '2.0', id: 'q', method, params }),
    });
    return res.json();
}

async function nearViewCall(method, args) {
    const json = await nearRpc('query', {
        request_type: 'call_function',
        finality: 'optimistic',
        account_id: config.contractId,
        method_name: method,
        args_base64: btoa(JSON.stringify(args)),
    });
    if (json.error) throw new Error(JSON.stringify(json.error));
    return JSON.parse(new TextDecoder().decode(new Uint8Array(json.result.result)));
}

async function nearFunctionCall(method, args) {
    // Get nonce and block hash
    const accessKeyRes = await nearRpc('query', {
        request_type: 'view_access_key',
        finality: 'optimistic',
        account_id: config.accountId,
        public_key: `ed25519:${config.publicKey}`,
    });
    if (accessKeyRes.error) throw new Error(JSON.stringify(accessKeyRes.error));
    const nonce = accessKeyRes.result.nonce + 1;
    const blockHash = accessKeyRes.result.block_hash;

    // Create signed transaction using WASM module
    const signedTxBase64 = create_signed_transaction(
        config.accountId,
        config.publicKey,
        config.privateKey,
        config.contractId,
        method,
        JSON.stringify(args),
        BigInt(nonce),
        blockHash,
        BigInt(300_000_000_000_000), // 300 TGas
        '0',
    );

    // Broadcast
    const broadcastRes = await nearRpc('broadcast_tx_commit', [signedTxBase64]);
    if (broadcastRes.error) {
        return { success: false, error: JSON.stringify(broadcastRes.error) };
    }
    const result = broadcastRes.result;
    if (result.status && result.status.SuccessValue !== undefined) {
        const decoded = atob(result.status.SuccessValue);
        return {
            success: true,
            result: decoded ? JSON.parse(decoded) : null,
            txHash: result.transaction.hash,
        };
    }
    return {
        success: false,
        error: JSON.stringify(result.status),
        txHash: result.transaction?.hash,
    };
}

// --- pkt-line helpers ---

function pktLineEncode(data) {
    const len = data.length + 4;
    const hex = len.toString(16).padStart(4, '0');
    const prefix = new TextEncoder().encode(hex);
    const result = new Uint8Array(prefix.length + data.length);
    result.set(prefix);
    result.set(data, prefix.length);
    return result;
}

function pktLineFlush() { return new TextEncoder().encode('0000'); }

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
        const headEntry = refs.find(([name]) => name === 'refs/heads/main')
            || refs.find(([name]) => name === 'refs/heads/master')
            || refs[0];
        const [headRef, headSha] = headEntry;
        const caps = `${CAPABILITIES} symref=HEAD:${headRef}`;
        parts.push(pktLineEncodeString(`${headSha} HEAD\0${caps}\n`));
        for (const [refName, sha] of refs) {
            parts.push(pktLineEncodeString(`${sha} ${refName}\n`));
        }
    }
    parts.push(pktLineFlush());

    return new Response(concatUint8Arrays(parts), {
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
        // Parse packfile using WASM module
        const parseJson = parse_packfile(rest);
        const parseResult = JSON.parse(parseJson);
        if (parseResult.error) {
            return makeReceivePackResponse([`ng unpack ${parseResult.error}`]);
        }

        let allObjects = parseResult.objects.map(obj => ({
            obj_type: obj.obj_type,
            data: Uint8Array.from(atob(obj.data), c => c.charCodeAt(0)),
        }));

        // Resolve deltas
        if (parseResult.deltas && parseResult.deltas.length > 0) {
            const localMap = {};
            for (const obj of allObjects) {
                const sha = git_sha1(obj.obj_type, obj.data);
                localMap[sha] = obj;
            }

            for (const delta of parseResult.deltas) {
                const deltaData = Uint8Array.from(atob(delta.delta_data), c => c.charCodeAt(0));
                let base = localMap[delta.base_sha];
                if (!base) {
                    // Fetch base object from archival transaction
                    const locations = await nearViewCall('get_object_locations', { shas: [delta.base_sha] });
                    if (locations[0] && locations[0][1]) {
                        const txObjects = await fetchObjectsFromTx(locations[0][1], config.accountId);
                        for (const obj of txObjects) {
                            const data = Uint8Array.from(atob(obj.data), c => c.charCodeAt(0));
                            const objSha = git_sha1(obj.obj_type, data);
                            localMap[objSha] = { obj_type: obj.obj_type, data };
                        }
                        base = localMap[delta.base_sha];
                    }
                }
                if (!base) {
                    return makeReceivePackResponse([`ng unpack base ${delta.base_sha} not found`]);
                }
                const resolved = apply_delta(base.data, deltaData);
                const obj = { obj_type: base.obj_type, data: resolved };
                const sha = git_sha1(obj.obj_type, obj.data);
                localMap[sha] = obj;
                allObjects.push(obj);
            }
        }

        // Push objects to contract (signed locally)
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

/// Fetch git objects from a push_objects transaction via archival RPC.
async function fetchObjectsFromTx(txHash, signerId) {
    const resp = await fetch(config.rpcUrl, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
            jsonrpc: '2.0', id: 1,
            method: 'EXPERIMENTAL_tx_status',
            params: {
                tx_hash: txHash,
                sender_account_id: signerId,
                wait_until: 'EXECUTED',
            },
        }),
    });
    const data = await resp.json();
    const actions = data.result?.transaction?.actions || [];
    const objects = [];

    for (const action of actions) {
        const fc = action.FunctionCall;
        if (fc && fc.method_name === 'push_objects') {
            const argsBytes = Uint8Array.from(atob(fc.args), c => c.charCodeAt(0));
            const args = JSON.parse(new TextDecoder().decode(argsBytes));
            for (const obj of (args.objects || [])) {
                objects.push(obj);
            }
        }
    }
    return objects;
}

/// Extract child SHAs from a git object for graph walking.
function extractChildren(objType, data) {
    const children = [];
    const decoder = new TextDecoder();
    if (objType === 'commit') {
        for (const line of decoder.decode(data).split('\n')) {
            if (line.startsWith('tree ')) children.push(line.slice(5).trim());
            else if (line.startsWith('parent ')) children.push(line.slice(7).trim());
            else if (line === '') break;
        }
    } else if (objType === 'tree') {
        let pos = 0;
        while (pos < data.length) {
            const nullPos = data.indexOf(0, pos);
            if (nullPos === -1 || nullPos + 21 > data.length) break;
            const childSha = Array.from(data.slice(nullPos + 1, nullPos + 21))
                .map(b => b.toString(16).padStart(2, '0')).join('');
            children.push(childSha);
            pos = nullPos + 21;
        }
    }
    return children;
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
        return new Response(
            concatUint8Arrays([pktLineEncodeString('NAK\n'), pktLineFlush()]),
            { headers: { 'Content-Type': 'application/x-git-upload-pack-result' } },
        );
    }

    // Walk object graph, fetching objects from archival transactions
    const havesSet = new Set(haves);
    const visitedShas = new Set();
    const visitedTxs = new Set();
    const packObjects = []; // { obj_type, data (base64) }
    const objectMap = new Map(); // sha -> { obj_type, data (Uint8Array) }
    const queue = [...wants];

    // Get signer ID from contract for archival RPC queries
    const owner = config.accountId;

    while (queue.length > 0) {
        // Collect batch of needed SHAs
        const batch = [];
        while (queue.length > 0 && batch.length < 50) {
            const sha = queue.shift();
            if (visitedShas.has(sha) || havesSet.has(sha)) continue;
            visitedShas.add(sha);
            batch.push(sha);
        }
        if (batch.length === 0) continue;

        // Get tx locations
        const locations = await nearViewCall('get_object_locations', { shas: batch });

        for (const [, txHash] of locations) {
            if (!txHash || visitedTxs.has(txHash)) continue;
            visitedTxs.add(txHash);

            // Fetch objects from the transaction
            const txObjects = await fetchObjectsFromTx(txHash, owner);
            for (const obj of txObjects) {
                const data = Uint8Array.from(atob(obj.data), c => c.charCodeAt(0));
                const objSha = git_sha1(obj.obj_type, data);

                if (!objectMap.has(objSha)) {
                    objectMap.set(objSha, { obj_type: obj.obj_type, data });
                    packObjects.push({ obj_type: obj.obj_type, data: obj.data });

                    // Walk children
                    for (const child of extractChildren(obj.obj_type, data)) {
                        if (!visitedShas.has(child) && !havesSet.has(child)) {
                            queue.push(child);
                        }
                    }
                }
            }
        }
    }

    const packData = build_packfile(JSON.stringify(packObjects));

    const nak = pktLineEncodeString('NAK\n');
    return new Response(concatUint8Arrays([nak, packData]), {
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
