# near-git-storage

A NEAR smart contract that stores Git repositories on the NEAR blockchain. Git object data (blobs, trees, commits) is stored directly in contract state, and the contract implements the storage backend for the Git smart HTTP protocol.

Designed as a decentralized storage backend for [wasm-git](https://github.com/petersalomonsen/wasm-git) web applications where Git is used as an application data store.

## Two ways to use it

### 1. HTTP git server (sandbox/development)

An [axum](https://github.com/tokio-rs/axum)-based HTTP server that implements the Git smart HTTP protocol and talks to a local [NEAR sandbox](https://github.com/near/near-sandbox-rs). Standard git clients work out of the box:

```
git clone http://localhost:8080/repo
cd repo
echo "hello" > file.txt && git add . && git commit -m "init"
git push origin master
```

### 2. Browser service worker (testnet/mainnet)

A service worker that intercepts Git HTTP requests from [wasm-git](https://github.com/petersalomonsen/wasm-git) and translates them into NEAR RPC calls — no server required. Transaction signing happens in the browser using a WASM module (ed25519 + borsh), so the private key never leaves the client.

```
Browser (wasm-git + OPFS)
    |
    |  git smart HTTP protocol
    |  (clone, fetch, push)
    v
Service Worker (near-git-sw.js)
    |  intercepts *.git/* requests
    |  uses WASM module for:
    |    - packfile parsing/building
    |    - NEAR transaction signing
    |
    v
NEAR RPC (testnet or mainnet)
    |
    v
Smart Contract (near-git-storage)
    |
    +-- refs: Map<refname, SHA>
    +-- object_types: Map<SHA, type>
    +-- object_data: Map<SHA, bytes>
    +-- object_txs: Map<SHA, TxHash>
```

## Architecture

### Contract storage

```rust
#[near(contract_state)]
pub struct GitStorage {
    /// Branch/tag pointers: "refs/heads/main" -> "abc123..."
    refs: IterableMap<String, SHA>,

    /// Object locations: SHA -> transaction hash (for archival lookup)
    object_txs: IterableMap<SHA, TxHash>,

    /// Object types: SHA -> "blob" | "tree" | "commit" | "tag"
    object_types: LookupMap<SHA, String>,

    /// Object data: SHA -> raw git object bytes
    object_data: LookupMap<SHA, Vec<u8>>,

    /// Repo owner (only owner can push)
    owner: AccountId,
}
```

### Data flow

**Push:**

1. Client parses packfile into individual git objects
2. Calls `push_objects(objects)` — contract computes SHA-1 for each, stores type + data, returns SHAs
3. Calls `register_push(tx_hash, object_shas, ref_updates)` — contract stores SHA -> tx_hash mappings and updates refs (with compare-and-swap)

**Clone/fetch:**

1. Client calls `get_refs()` to discover branches
2. Walks the object graph (commit -> tree -> blob) by calling `get_objects(shas)`
3. Builds a packfile from the retrieved objects and returns it to wasm-git

### Contract methods

```rust
/// Store git objects. Computes SHA-1, stores type + data in contract state.
pub fn push_objects(&mut self, objects: Vec<GitObject>) -> PushObjectsResult

/// Register a push transaction and update refs (compare-and-swap).
pub fn register_push(&mut self, tx_hash: TxHash, object_shas: Vec<SHA>, ref_updates: Vec<RefUpdate>)

/// Return all refs (view call, free).
pub fn get_refs(&self) -> Vec<(String, SHA)>

/// Return transaction locations for requested objects (view call, free).
pub fn get_object_locations(&self, shas: Vec<SHA>) -> Vec<(SHA, Option<TxHash>)>

/// Retrieve stored objects by SHA (view call, free).
pub fn get_objects(&self, shas: Vec<SHA>) -> Vec<(SHA, Option<GitObject>)>
```

## Project structure

```
near-git-storage/
  src/lib.rs            # NEAR smart contract
  git-core/             # Shared Rust crate: packfile + pkt-line parsing
  git-server/           # HTTP git server (axum + NEAR sandbox)
  wasm-lib/             # WASM module: packfile ops + NEAR tx signing
  e2e/
    public/
      index.html        # HTTP server demo
      index-sw.html     # Service worker demo (sandbox)
      testnet.html      # Service worker demo (testnet)
      near-git-sw.js    # Service worker (standalone, uses WASM)
      worker.js         # wasm-git OPFS worker
      wasm-lib/         # Built WASM module
    tests/
      http-server.spec.js      # E2E: push + re-clone via HTTP server
      service-worker.spec.js   # E2E: push + re-clone via service worker
```

## Getting started

### Prerequisites

- Rust (1.86+ for NEAR wasm compatibility)
- [cargo-near](https://github.com/near/cargo-near)
- Node.js
- [wasm-pack](https://rustwasm.github.io/wasm-pack/) (for building the browser WASM module)

### Build the contract

```bash
./build.sh
```

### Run the HTTP git server (local sandbox)

```bash
cargo run -p git-server
```

This starts a NEAR sandbox, deploys the contract, and serves the git HTTP protocol on `http://localhost:8080/repo`.

### Run the testnet demo

```bash
cd e2e
npm run setup   # copies wasm-git assets
node serve.mjs
```

Open `http://localhost:8081/testnet` and enter your testnet credentials.

### Run the E2E tests

```bash
cd e2e
npx playwright test
```

This starts the git server automatically (fresh sandbox) and runs both the HTTP server and service worker tests.

### Build the WASM module

```bash
cd wasm-lib
wasm-pack build --target web --out-dir ../e2e/public/wasm-lib
```

## Archival RPC

The contract stores a mapping from object SHA to the transaction hash of the `push_objects` call that created it (`object_txs`). This allows retrieving git object data from NEAR archival nodes as an alternative to reading from contract state — useful for reducing storage costs in the future.

- **Provider**: [FastNEAR](https://fastnear.com) archival RPC (`https://archival-rpc.testnet.fastnear.com` for testnet)

## Related projects

- [wasm-git](https://github.com/petersalomonsen/wasm-git) — Git compiled to WebAssembly
- [wasm-git-apps](https://github.com/petersalomonsen/wasm-git-apps) — Devcontainer for AI-driven web app creation using wasm-git
- [gitoxide](https://github.com/Byron/gitoxide) — Pure Rust Git implementation
- [NEAR Protocol](https://near.org) — Layer 1 blockchain with WebAssembly smart contracts

Sources:
- [near-sandbox-rs](https://github.com/near/near-sandbox-rs)
- [near-workspaces-rs](https://github.com/near/near-workspaces-rs)
- [NEAR Integration Tests](https://docs.near.org/sdk/rust/testing/integration-tests)

## License

MIT
