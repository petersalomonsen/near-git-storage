/**
 * Mock wallet for NearConnect in E2E tests.
 *
 * Intercepts the NearConnect manifest to inject a mock wallet that signs
 * transactions using the sandbox genesis key via the git-server's RPC proxy.
 */

export const MOCK_MANIFEST_ID = 'mock-wallet';

/**
 * The mock wallet executor JS. Runs inside NearConnect's sandboxed iframe.
 *
 * For signAndSendTransaction, it uses the near-api-js UMD bundle loaded
 * via importScripts (available in the sandbox iframe context) to sign and
 * broadcast transactions with the genesis key.
 */
export function createMockExecutorJs(rpcUrl) {
    return `(function() {
  const RPC_URL = '${rpcUrl}';
  const GENESIS_PRIVATE_KEY = 'ed25519:3tgdk2wPraJzT4nsTuf86UX41xgPNk3MHnq8epARMdBNs29AFEztAuaQ7iHddDfXG9F2RzV1XNQYgJyAyoW51UBB';
  const GENESIS_PUBLIC_KEY = 'ed25519:5BGSaf6YjVm7565VzWQHNxoyEjwr3jUpRJSGjREvU9dB';

  window.selector.ready({
    async signIn({ network }) {
      const a = window.sandboxedLocalStorage.getItem('signedAccountId') || '';
      return a ? [{ accountId: a, publicKey: GENESIS_PUBLIC_KEY }] : [];
    },
    async signOut() {
      window.sandboxedLocalStorage.removeItem('signedAccountId');
    },
    async getAccounts({ network }) {
      const a = window.sandboxedLocalStorage.getItem('signedAccountId');
      if (!a) return [];
      return [{ accountId: a, publicKey: GENESIS_PUBLIC_KEY }];
    },
    async verifyOwner() { throw new Error('Not supported'); },
    async signMessage()  { throw new Error('Not supported'); },
    async signAndSendTransaction(p) {
      const accountId = window.sandboxedLocalStorage.getItem('signedAccountId') || '';

      // Use the test signing endpoint on the git-server
      const resp = await fetch('/_test/sign-and-send', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          signerId: accountId,
          receiverId: p.receiverId,
          actions: p.actions,
        }),
      });
      if (!resp.ok) {
        const text = await resp.text();
        throw new Error('Sign and send failed: ' + text);
      }
      return resp.json();
    },
    async signAndSendTransactions(p) {
      const results = [];
      for (const tx of (p.transactions || [])) {
        const result = await this.signAndSendTransaction(tx);
        results.push(result);
      }
      return results;
    },
    async signDelegateActions(p) { throw new Error('Not supported'); },
  });
})();`;
}

export function createMockManifest(executorUrl) {
    return {
        wallets: [{
            id: MOCK_MANIFEST_ID,
            name: 'Mock Wallet',
            icon: "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg'/>",
            website: 'https://example.com',
            description: 'Mock wallet for sandbox testing',
            version: '1.0.0',
            type: 'sandbox',
            executor: executorUrl || '/_mock-wallet/executor.js',
            features: { signAndSendTransaction: true },
            permissions: { allowsOpen: false },
        }],
    };
}

/**
 * Set up mock wallet routes on a Playwright page/context.
 * Intercepts NearConnect manifest URLs and serves our mock executor.
 */
export async function setupMockWallet(context, accountId, rpcUrl = '/near-rpc') {
    const manifest = createMockManifest('/_mock-wallet/executor.js');
    const executorJs = createMockExecutorJs(rpcUrl);

    // Intercept manifest CDN URLs
    await context.route('**/raw.githubusercontent.com/**manifest.json*', route =>
        route.fulfill({ body: JSON.stringify(manifest), contentType: 'application/json' })
    );
    await context.route('**/cdn.jsdelivr.net/**manifest.json*', route =>
        route.fulfill({ body: JSON.stringify(manifest), contentType: 'application/json' })
    );

    // Serve mock executor
    await context.route('**/_mock-wallet/executor.js', route =>
        route.fulfill({ body: executorJs, contentType: 'application/javascript' })
    );
}

/**
 * Pre-seed localStorage so the mock wallet is auto-connected on page load.
 */
export function seedWalletScript(accountId) {
    return ({ walletId, acct }) => {
        localStorage.setItem('selected-wallet', walletId);
        localStorage.setItem(`${walletId}:signedAccountId`, acct);
    };
}
