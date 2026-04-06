# near-git-storage

A NEAR smart contract that stores Git repositories on the NEAR blockchain using packfiles with native delta compression. Storage costs are minimal — a 2.1 MB repo stores in ~40 KB on-chain, and incremental edits cost ~400 bytes.

Designed as a decentralized storage backend for [wasm-git](https://github.com/petersalomonsen/wasm-git) web applications where Git is used as an application data store.

## Three ways to use it

### 1. git-remote-near (CLI)

A Git remote helper that enables `git push/clone` directly to NEAR:

```
cargo install --path git-remote-near

git clone near://myrepo.gitfactory.testnet
git push near://myrepo.gitfactory.testnet main
```

Uses `git pack-objects --thin --revs` for native delta compression. Incremental pushes produce thin packs (~336 bytes for a 1-line edit to a 64 KB file).

### 2. Browser service worker (testnet/mainnet)

A service worker that intercepts Git HTTP requests from [wasm-git](https://github.com/petersalomonsen/wasm-git) and translates them into NEAR RPC calls — no server required. Transaction signing happens in the browser using a WASM module (ed25519 + borsh), so the private key never leaves the client.

### 3. HTTP git server (sandbox/development)

An [axum](https://github.com/tokio-rs/axum)-based HTTP server for local development with a NEAR sandbox.

## Architecture

```
Browser (wasm-git + OPFS)  /  git CLI
    |                          |
    v                          v
Service Worker             git-remote-near
    |  packfile building       |  git pack-objects --thin
    |  NEAR tx signing         |  near-api
    v                          v
NEAR RPC (testnet or mainnet)
    |
    v
Smart Contract (near-git-storage)
    |
    +-- refs: Map<refname, SHA>
    +-- packs: Map<index, packfile_bytes>
    +-- pack_count: u32
```

### Contract storage

The contract is a minimal key-value store for packfiles:

```rust
pub struct GitStorage {
    refs: IterableMap<String, String>,    // branch/tag pointers
    packs: LookupMap<u32, Vec<u8>>,       // packfiles, one per push
    pack_count: u32,                      // number of stored packs
    owner: AccountId,                     // only owner can push
}
```

### Data flow

**Push:**

1. Client builds a packfile with delta compression (`git pack-objects` or `build_packfile_with_bases`)
2. Calls `push(pack_data, ref_updates)` — contract stores packfile verbatim and updates refs (CAS)

**Clone/fetch:**

1. Client calls `get_refs()` to discover branches
2. Calls `get_packs(from_index)` to retrieve packfiles
3. Feeds packs to `git index-pack --fix-thin` (CLI) or parses them in WASM (browser)

### Contract methods

```rust
// Store a packfile and update refs (borsh-serialized)
pub fn push(&mut self, pack_data: Vec<u8>, ref_updates: Vec<RefUpdate>)

// Return all refs (JSON, view call)
pub fn get_refs(&self) -> Vec<(String, String)>

// Return number of stored packfiles (JSON, view call)
pub fn get_pack_count(&self) -> u32

// Retrieve packfiles from index (borsh, view call)
pub fn get_packs(&self, from_index: u32) -> Vec<Vec<u8>>

// Clear all storage (call before self_delete for large repos)
pub fn clear_storage(&mut self)

// Delete account, send funds to owner
pub fn self_delete(&mut self) -> Promise
```

### Factory & global contracts

Repos are deployed as sub-accounts of a factory (e.g. `myrepo.gitfactory.testnet`) using hash-based global contracts. Each repo is pinned to the exact contract version it was created with — no surprise upgrades. Users migrate by creating a new repo and deleting the old one.

The contract charges a 0.1 NEAR service fee on `new()` to the `FEE_RECIPIENT` (build-time env var) to cover global contract deployment costs.

## Project structure

```
near-git-storage/
  src/lib.rs            # NEAR smart contract (packfile store)
  factory/              # Factory contract (creates repos as sub-accounts)
  git-core/             # Shared crate: packfile parse/build, delta, zlib
  git-remote-near/      # CLI git remote helper
  git-server/           # HTTP git server (axum + NEAR sandbox)
  wasm-lib/             # Browser WASM: packfile ops + NEAR tx signing
  e2e/
    public/
      near-git-sw.js    # Service worker
      wasm-lib/         # Built WASM module
    tests/              # Playwright e2e tests
```

## Getting started

### Prerequisites

- Rust (1.86+ for NEAR wasm compatibility)
- [cargo-near](https://github.com/near/cargo-near)
- Node.js
- [wasm-pack](https://rustwasm.github.io/wasm-pack/) (for building the browser WASM module)

### Build

```bash
# Build contracts (default: FEE_RECIPIENT=gitfactory.testnet)
./build.sh

# For mainnet:
FEE_RECIPIENT=gitfactory.near ./build.sh

# Build WASM module for browser
cd wasm-lib && wasm-pack build --target web

# Install git-remote-near
cargo install --path git-remote-near
```

### Run tests

```bash
# Integration + factory tests (starts sandbox automatically)
cargo test --test integration --test factory

# Playwright e2e tests (starts git-server + sandbox)
cd e2e && npx playwright test
```

### Testnet deployments

- `gitfactory.testnet` — factory contract
- Web4 UI: https://gitfactory.testnet.page/
- Cloudflare Pages: https://near-git-storage.pages.dev/create-repo

## Compression results

| Metric | Raw objects | Packfile storage |
|--------|-----------|-----------------|
| psalomo2026 repo (139 objects) | 2.1 MB | **40 KB** |
| 1-line edit to 64 KB file | 11 KB | **~400 bytes** |
| Storage cost estimate | ~21 NEAR | **~0.5 NEAR** |

## Related projects

- [wasm-git](https://github.com/petersalomonsen/wasm-git) — Git compiled to WebAssembly
- [NEAR Protocol](https://near.org) — Layer 1 blockchain with WebAssembly smart contracts

## License

MIT
