/**
 * Mock wallet for NearConnect in E2E tests.
 *
 * Intercepts the NearConnect manifest to inject a mock wallet that signs
 * transactions directly using the sandbox genesis key. This avoids needing
 * a real wallet extension or popup flow.
 */

export const MOCK_MANIFEST_ID = 'mock-wallet';

/**
 * The mock wallet executor JS. Runs inside NearConnect's sandboxed iframe.
 * For signAndSendTransaction, it signs and broadcasts directly to sandbox RPC
 * using the genesis private key.
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
      try {
        // Import near-api-js from CDN for signing
        const { connect, keyStores, KeyPair, utils } = await import('https://cdn.jsdelivr.net/npm/near-api-js@5.1.1/+esm');

        const keyStore = new keyStores.InMemoryKeyStore();
        await keyStore.setKey('sandbox', accountId, KeyPair.fromString(GENESIS_PRIVATE_KEY));

        const near = await connect({
          networkId: 'sandbox',
          nodeUrl: RPC_URL,
          keyStore,
        });

        const account = await near.account(accountId);

        const nearActions = (p.actions || []).map(a => {
          if (a.type === 'FunctionCall') {
            const args = typeof a.params.args === 'object' && !(a.params.args instanceof Uint8Array)
              ? JSON.stringify(a.params.args)
              : a.params.args;
            return {
              type: 'FunctionCall',
              params: {
                methodName: a.params.methodName,
                args: typeof args === 'string' ? new TextEncoder().encode(args) : args,
                gas: BigInt(a.params.gas || '100000000000000'),
                deposit: BigInt(a.params.deposit || '0'),
              }
            };
          }
          return a;
        });

        const result = await account.signAndSendTransaction({
          receiverId: p.receiverId,
          actions: nearActions,
        });

        return result;
      } catch (err) {
        console.error('[mock-wallet] signAndSendTransaction error:', err);
        throw err;
      }
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
