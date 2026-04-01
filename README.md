# near-git-storage

A NEAR smart contract that stores Git repositories on the NEAR blockchain using an **archival transaction** approach: git object data is embedded in transaction payloads but never stored in contract state. The contract only stores lightweight pointers (refs → SHA → transaction IDs), and the actual data is retrieved from NEAR archival RPC nodes.

Designed as a decentralized storage backend for [wasm-git](https://github.com/petersalomonsen/wasm-git) web applications where Git is used as an application data store.

## Why the archival approach?

NEAR contract storage costs ~0.01 NEAR per KB. For a repo with 100 KB of data, that's 1 NEAR (~$5) locked as storage deposit. The archival approach avoids this:

| Approach | Cost for 100 KB of git objects |
|---|---|
| Contract state storage | ~1 NEAR (storage deposit, locked) |
| Archival transactions | ~0.001 NEAR (gas only) + ~0.001 NEAR (pointer storage) |

The trick: NEAR archival nodes keep full transaction history forever. We post git objects as transaction payloads, but the contract discards the data after computing the SHA. Only the SHA → transaction ID mapping is stored (52 bytes per object). To retrieve the data later, we fetch the original transaction from an archival RPC node.

## Architecture

```
Browser (wasm-git + OPFS)
    │
    │  git smart HTTP protocol
    │  (clone, fetch, push)
    │
    ▼
Service Worker
    │  intercepts *.git/* requests
    │  translates to NEAR RPC calls
    │
    ├──────────────────────────────────────────┐
    ▼                                          ▼
NEAR Smart Contract                    NEAR Archival RPC
    │                                          │
    ├── refs: Map<refname, SHA>                │  tx(hash, sender) → full transaction
    ├── objects: Map<SHA, TxHash>  ────────────┘  including function call args
    └── (no object data stored)                   (= git object bytes)
```

### Data flow

**Push (store objects):**

```
1. Client packs git objects into transaction args
   → calls contract.push_objects(objects: Vec<GitObject>)
   → contract computes SHA for each, discards data
   → client receives tx_hash from RPC response

2. Client registers the transaction
   → calls contract.register_push(tx_hash, shas: Vec<SHA>, ref_updates)
   → contract stores SHA → tx_hash mappings (52 bytes each)
   → contract updates refs
```

**Clone/fetch (retrieve objects):**

```
1. Client asks for refs
   → calls contract.get_refs() (view call, free)

2. Client asks which objects it needs
   → calls contract.get_object_locations(shas: Vec<SHA>) (view call, free)
   → returns Vec<(SHA, TxHash)>

3. Service worker fetches each transaction from archival RPC
   → POST archival-rpc.near.org
     {"method": "tx", "params": {"tx_hash": "...", "sender_id": "..."}}
   → extracts function call args → git object bytes

4. Service worker assembles objects into packfile response for wasm-git
```

## Contract storage (minimal)

```rust
/// Only pointers — actual data lives in transaction history
#[near(contract_state)]
pub struct GitStorage {
    /// Branch/tag pointers: refname → SHA (e.g., "refs/heads/main" → "abc123...")
    refs: UnorderedMap<String, SHA>,

    /// Object locations: SHA → transaction hash where the data was posted
    object_txs: UnorderedMap<SHA, CryptoHash>,

    /// Repo owner (only owner can push)
    owner: AccountId,
}
```

Storage per object: **52 bytes** (20-byte SHA + 32-byte tx hash).
A repo with 1000 objects costs ~0.0005 NEAR in storage deposit.

## Contract methods

```rust
/// Store git objects in a transaction. The contract validates SHAs
/// but does NOT store the object data — only computes and returns SHAs.
/// The data persists in the transaction's function call args on archival nodes.
pub fn push_objects(&mut self, objects: Vec<GitObject>) -> Vec<SHA>

/// Register a previous push_objects transaction and update refs.
/// Called after push_objects, with the tx_hash from that transaction.
pub fn register_push(
    &mut self,
    tx_hash: CryptoHash,
    object_shas: Vec<SHA>,
    ref_updates: Vec<RefUpdate>,
)

/// Return all refs (view call, free)
pub fn get_refs(&self) -> Vec<(String, SHA)>

/// Return transaction locations for requested objects (view call, free)
pub fn get_object_locations(&self, shas: Vec<SHA>) -> Vec<(SHA, CryptoHash)>

/// Get the object graph reachable from a set of SHAs (for fetch negotiation)
/// Returns the SHAs of all objects needed (view call, free)
pub fn resolve_wants(
    &self,
    wants: Vec<SHA>,
    haves: Vec<SHA>,
) -> Vec<SHA>
```

## Service worker

The service worker translates between the git smart HTTP protocol and NEAR RPC calls:

```javascript
self.addEventListener('fetch', (event) => {
    const url = new URL(event.request.url);
    if (!url.pathname.includes('.git/')) return;

    if (url.pathname.endsWith('/info/refs')) {
        // View call: contract.get_refs()
        // Format as git pkt-line response
    }
    else if (url.pathname.endsWith('/git-upload-pack')) {
        // 1. Parse wants/haves from request body
        // 2. View call: contract.resolve_wants(wants, haves)
        // 3. View call: contract.get_object_locations(needed_shas)
        // 4. Fetch each tx from archival RPC, extract object bytes
        // 5. Assemble into packfile, return to wasm-git
    }
    else if (url.pathname.endsWith('/git-receive-pack')) {
        // 1. Parse packfile from request body into individual objects
        // 2. Call: contract.push_objects(objects) → get tx_hash
        // 3. Call: contract.register_push(tx_hash, shas, ref_updates)
        // 4. Return success/failure to wasm-git
    }
});
```

## Object graph walking

For `resolve_wants`, the contract needs to know the structure of commits and trees (which objects reference which). Since object data isn't stored in contract state, there are two options:

**Option A: Store a lightweight object graph in contract state.**
During `push_objects`, parse just enough to record parent relationships:
- Commit → parent commit SHAs + tree SHA
- Tree → child blob/tree SHAs

This adds ~40-60 bytes per object but enables the contract to walk the graph for fetch negotiation.

**Option B: Let the service worker walk the graph.**
The service worker fetches objects from archival RPC, parses them locally, and determines what's needed. The contract only provides ref → SHA and SHA → tx_hash mappings. Simpler contract, more work in the service worker.

## Testing with near-sandbox

The project uses [near-workspaces-rs](https://github.com/near/near-workspaces-rs) with [near-sandbox-rs](https://github.com/near/near-sandbox-rs) for local testing. Tests run against a local NEAR sandbox node — no testnet needed.

```rust
#[tokio::test]
async fn test_push_and_fetch() {
    let worker = near_workspaces::sandbox().await.unwrap();
    let contract = worker.dev_deploy(WASM_BYTES).await.unwrap();

    // Push objects
    let result = contract.call("push_objects")
        .args_json(json!({"objects": [{"type": "blob", "data": "..."}]}))
        .transact().await.unwrap();
    let tx_hash = result.outcome().transaction_hash;

    // Register push
    contract.call("register_push")
        .args_json(json!({
            "tx_hash": tx_hash,
            "object_shas": ["abc123..."],
            "ref_updates": [{"name": "refs/heads/main", "new_sha": "abc123..."}]
        }))
        .transact().await.unwrap();

    // Verify refs
    let refs: Vec<(String, String)> = contract.view("get_refs")
        .await.unwrap().json().unwrap();
    assert_eq!(refs[0].0, "refs/heads/main");
}
```

## Archival RPC considerations

- **Providers**: NEAR Foundation, Pagoda, Lava Network, or self-hosted archival nodes
- **Reliability**: archival nodes keep all transaction history, but availability depends on the provider
- **Latency**: fetching from archival RPC is slower than direct state reads; consider caching fetched objects in the browser's OPFS or an R2 cache layer
- **Fallback**: for critical data, optionally mirror to Cloudflare R2 for fast reads

## Related projects

- [wasm-git](https://github.com/petersalomonsen/wasm-git) — Git compiled to WebAssembly
- [wasm-git-apps](https://github.com/petersalomonsen/wasm-git-apps) — Devcontainer for AI-driven web app creation using wasm-git
- [r2-git-server](https://github.com/petersalomonsen/r2-git-server) — Serverless Git server on Cloudflare Workers + R2
- [gitoxide](https://github.com/Byron/gitoxide) — Pure Rust Git implementation
- [NEAR Protocol](https://near.org) — Layer 1 blockchain with WebAssembly smart contracts

Sources:
- [near-sandbox-rs](https://github.com/near/near-sandbox-rs)
- [near-workspaces-rs](https://github.com/near/near-workspaces-rs)
- [NEAR Integration Tests](https://docs.near.org/sdk/rust/testing/integration-tests)

## License

MIT
