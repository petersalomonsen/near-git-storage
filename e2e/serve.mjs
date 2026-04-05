import http from 'http';
import fs from 'fs';
import path from 'path';
import { createHash } from 'crypto';
import { KeyPair } from '@near-js/crypto';
import { actionCreators, createTransaction, Signature, SignedTransaction } from '@near-js/transactions';
import bs58 from 'bs58';

const PORT = process.env.WEB_APP_PORT || 8081;
const GIT_SERVER_PORT = process.env.GIT_SERVER_PORT || 8080;
// For the service worker variant, NEAR RPC is proxied
// Dynamically discovered from git server's /near-info
let nearRpcUrl = null;

async function resolveNearRpcUrl() {
    if (nearRpcUrl) return nearRpcUrl;
    try {
        const res = await fetch(`http://localhost:${GIT_SERVER_PORT}/near-info`);
        const info = await res.json();
        nearRpcUrl = info.rpcUrl;
        console.log(`Discovered NEAR RPC URL: ${nearRpcUrl}`);
        return nearRpcUrl;
    } catch (e) {
        return null;
    }
}

const MIME = {
    '.html': 'text/html; charset=utf-8',
    '.js': 'application/javascript',
    '.mjs': 'application/javascript',
    '.wasm': 'application/wasm',
    '.css': 'text/css',
    '.json': 'application/json',
    '.txt': 'text/plain',
};

function proxyToGit(req, res) {
    const proxyReq = http.request({
        hostname: 'localhost',
        port: GIT_SERVER_PORT,
        path: req.url,
        method: req.method,
        headers: req.headers,
    }, (proxyRes) => {
        // Copy response headers and add CORS
        const headers = { ...proxyRes.headers };
        headers['access-control-allow-origin'] = '*';
        headers['access-control-allow-methods'] = 'GET, POST, OPTIONS';
        headers['access-control-allow-headers'] = '*';
        headers['access-control-expose-headers'] = '*';
        res.writeHead(proxyRes.statusCode, headers);
        proxyRes.pipe(res);
    });
    proxyReq.on('error', (err) => {
        res.writeHead(502);
        res.end('Git server error: ' + err.message);
    });
    req.pipe(proxyReq);
}

async function proxyToNearRpc(req, res) {
    const rpcUrl = await resolveNearRpcUrl();
    if (!rpcUrl) {
        res.writeHead(503);
        res.end('NEAR RPC not configured');
        return;
    }
    const parsed = new URL(rpcUrl);
    const proxyReq = http.request({
        hostname: parsed.hostname,
        port: parsed.port,
        path: '/',
        method: req.method,
        headers: {
            'content-type': 'application/json',
        },
    }, (proxyRes) => {
        const headers = { ...proxyRes.headers };
        headers['access-control-allow-origin'] = '*';
        res.writeHead(proxyRes.statusCode, headers);
        proxyRes.pipe(res);
    });
    proxyReq.on('error', (err) => {
        res.writeHead(502);
        res.end('NEAR RPC error: ' + err.message);
    });
    req.pipe(proxyReq);
}

// Genesis key for sandbox signing
const GENESIS_PRIVATE_KEY = 'ed25519:3tgdk2wPraJzT4nsTuf86UX41xgPNk3MHnq8epARMdBNs29AFEztAuaQ7iHddDfXG9F2RzV1XNQYgJyAyoW51UBB';
const GENESIS_KEY_PAIR = KeyPair.fromString(GENESIS_PRIVATE_KEY);

async function signAndSendTransaction({ signerId, receiverId, actions }) {
    const rpcUrl = await resolveNearRpcUrl();
    if (!rpcUrl) throw new Error('Sandbox RPC not available');

    const rpc = async (method, params) => {
        const resp = await fetch(rpcUrl, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
        });
        return resp.json();
    };

    // Get access key nonce
    const keyData = await rpc('query', {
        request_type: 'view_access_key',
        finality: 'final',
        account_id: signerId,
        public_key: GENESIS_KEY_PAIR.getPublicKey().toString(),
    });
    if (!keyData.result) throw new Error(`No access key for ${signerId}`);
    const nonce = keyData.result.nonce + 1;
    const blockHash = bs58.decode(keyData.result.block_hash);

    // Build actions
    const nearActions = actions.map(a => {
        if (a.type === 'FunctionCall') {
            const args = typeof a.params.args === 'string'
                ? Buffer.from(a.params.args, 'base64')
                : Buffer.from(JSON.stringify(a.params.args));
            return actionCreators.functionCall(
                a.params.methodName,
                args,
                BigInt(a.params.gas || '100000000000000'),
                BigInt(a.params.deposit || '0'),
            );
        }
        throw new Error('Unsupported action type: ' + a.type);
    });

    const tx = createTransaction(signerId, GENESIS_KEY_PAIR.getPublicKey(), receiverId, nonce, nearActions, blockHash);
    const serialized = tx.encode();
    const hash = createHash('sha256').update(serialized).digest();
    const signed = GENESIS_KEY_PAIR.sign(hash);
    const sig = new Signature({ keyType: 0, data: signed.signature });
    const signedTx = new SignedTransaction({ transaction: tx, signature: sig });

    const result = await rpc('broadcast_tx_commit', [Buffer.from(signedTx.encode()).toString('base64')]);
    if (result.error) throw new Error(JSON.stringify(result.error));
    if (result.result?.status?.Failure) throw new Error(JSON.stringify(result.result.status.Failure));
    return result.result;
}

http.createServer((req, res) => {
    const urlPath = (req.url || '/').split('?')[0];

    // SharedArrayBuffer headers needed for wasm-git pthreads,
    // but skip for create-repo page which uses near-connect (COOP breaks popups)
    if (urlPath !== '/create-repo') {
        res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
        res.setHeader('Cross-Origin-Embedder-Policy', 'credentialless');
    }

    // Handle OPTIONS preflight
    if (req.method === 'OPTIONS') {
        res.setHeader('Access-Control-Allow-Origin', '*');
        res.setHeader('Access-Control-Allow-Methods', 'GET, POST, OPTIONS');
        res.setHeader('Access-Control-Allow-Headers', '*');
        res.writeHead(204);
        res.end();
        return;
    }

    // Health check
    if (urlPath === '/ping') {
        res.writeHead(200);
        res.end('pong');
        return;
    }

    // Proxy NEAR RPC requests
    if (urlPath === '/near-rpc') {
        proxyToNearRpc(req, res);
        return;
    }

    // Test endpoint: sign and send a transaction using genesis key
    if (urlPath === '/_test/sign-and-send' && req.method === 'POST') {
        let body = '';
        req.on('data', chunk => { body += chunk; });
        req.on('end', async () => {
            try {
                const result = await signAndSendTransaction(JSON.parse(body));
                res.writeHead(200, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify(result));
            } catch (err) {
                res.writeHead(500, { 'Content-Type': 'application/json' });
                res.end(JSON.stringify({ error: err.message }));
            }
        });
        return;
    }

    // Proxy NEAR endpoints to git server
    if (urlPath === '/near-info' || urlPath === '/near-call' || urlPath === '/near-credentials' || urlPath === '/parse-packfile') {
        proxyToGit(req, res);
        return;
    }

    // Proxy git smart-HTTP requests to git server
    if (/\.git\//.test(urlPath) || urlPath.match(/^\/repo\//)) {
        proxyToGit(req, res);
        return;
    }

    // Serve static files
    let filePath;
    if (urlPath === '/') {
        filePath = path.join('public', 'index.html');
    } else if (urlPath === '/sw-test') {
        filePath = path.join('public', 'index-sw.html');
    } else if (urlPath === '/testnet') {
        filePath = path.join('public', 'testnet.html');
    } else if (urlPath === '/create-repo') {
        filePath = path.join('public', 'create-repo.html');
    } else {
        filePath = path.join('public', urlPath);
    }

    fs.readFile(filePath, (err, data) => {
        if (err) {
            res.writeHead(404);
            res.end('Not found: ' + urlPath);
            return;
        }
        const ext = path.extname(filePath);
        res.writeHead(200, { 'Content-Type': MIME[ext] || 'application/octet-stream' });
        res.end(data);
    });
}).listen(PORT, () => console.log(`E2E app server on http://localhost:${PORT}/`));
