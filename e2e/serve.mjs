import http from 'http';
import fs from 'fs';
import path from 'path';

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

http.createServer((req, res) => {
    // Required for SharedArrayBuffer (wasm-git pthreads)
    res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
    res.setHeader('Cross-Origin-Embedder-Policy', 'credentialless');

    const urlPath = (req.url || '/').split('?')[0];

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
