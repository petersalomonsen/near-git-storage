# near-git-storage

A NEAR smart contract that implements the Git smart HTTP protocol, enabling decentralized Git repository storage on the NEAR blockchain. Designed as a storage backend for [wasm-git](https://github.com/petersalomonsen/wasm-git) web applications where Git is used as an application data store.

## Motivation

Web applications built with wasm-git use Git repositories as their data layer — every user action is a commit, data is versioned by default, and the app works offline via OPFS. But the Git server is still a centralized component.

This project replaces the centralized Git server with a NEAR smart contract, making the data layer fully decentralized:

- **User-owned data** — each user's data lives in a Git repo on-chain, controlled by their NEAR account
- **No server to run** — the smart contract is the Git server
- **Censorship resistant** — no single party can deny access to the data
- **Verifiable history** — the blockchain provides an immutable audit trail of all pushes
- **Git-native governance** — branch protection and required checks can be enforced at the contract level

## Architecture

```
Browser (wasm-git + OPFS)
    │
    │  git smart HTTP protocol
    │  (clone, fetch, push)
    │
    ▼
Service Worker
    │
    │  intercepts *.git/* requests
    │  translates to NEAR RPC calls
    │
    ▼
NEAR Smart Contract
    │
    ├── objects: Map<SHA, Vec<u8>>     (git objects: blobs, trees, commits)
    ├── refs: Map<String, SHA>         (branch/tag pointers)
    └── packs: Map<PackId, Vec<u8>>    (packfile chunks for transfer)
```

wasm-git thinks it's talking to a regular Git HTTP server. The service worker intercepts the requests and translates them into NEAR contract calls. The contract implements the core logic of `git-upload-pack` and `git-receive-pack`.

## How the Git smart HTTP protocol maps to contract methods

The Git smart HTTP protocol has 4 endpoints. Each maps to a contract method:

| Git HTTP endpoint | Direction | Contract method |
|---|---|---|
| `GET /repo.git/info/refs?service=git-upload-pack` | fetch/clone | `get_refs()` |
| `POST /repo.git/git-upload-pack` | fetch/clone | `upload_pack(wants, haves)` |
| `GET /repo.git/info/refs?service=git-receive-pack` | push | `get_refs()` |
| `POST /repo.git/git-receive-pack` | push | `receive_pack(pack_data, ref_updates)` |

### Contract methods

```rust
/// Return all refs (branch/tag pointers)
fn get_refs() -> Vec<(String, SHA)>

/// Handle fetch/clone — build a packfile of requested objects
/// Returns packfile bytes or URIs to packfile chunks
fn upload_pack(wants: Vec<SHA>, haves: Vec<SHA>) -> UploadPackResponse

/// Handle push — unpack received objects and update refs
fn receive_pack(pack_data: Vec<u8>, ref_updates: Vec<RefUpdate>)
```

### Packfile handling

The contract must parse and generate Git packfiles — the binary format Git uses to transfer objects efficiently.

**On push (`receive_pack`):**
1. Parse the incoming packfile into individual Git objects
2. Store each object by its SHA hash
3. Validate and apply ref updates

**On fetch (`upload_pack`):**
1. Walk the object graph from "wants" to "haves" to determine needed objects
2. Pack the needed objects into a packfile
3. Return the packfile (or chunk URIs if it exceeds transaction limits)

### Chunking for NEAR transaction limits

NEAR's max transaction size is **1.5 MB** (1,572,864 bytes as of protocol v69). For repos that exceed this:

**Push (large packfiles):** Split across multiple contract calls, each under 1.5 MB. The contract reassembles them before processing.

**Fetch (large responses):** Use Git protocol v2's `packfile-uris` feature. Instead of returning the full packfile, the contract returns URIs to individual chunks:

```
packfile-uris:
  sha1-of-pack-1  near://contract.near/pack/chunk-1
  sha1-of-pack-2  near://contract.near/pack/chunk-2
```

The service worker fetches each chunk via separate view calls (free, no gas cost for reads). For typical small app repos (JSON entries, a few hundred KB), a single call is sufficient.

## Service worker

The service worker intercepts Git HTTP requests from wasm-git and translates them to NEAR RPC calls:

```javascript
self.addEventListener('fetch', (event) => {
    const url = new URL(event.request.url);

    if (url.pathname.endsWith('/info/refs')) {
        // Call contract.get_refs()
        // Format response as git pkt-line
    }
    else if (url.pathname.endsWith('/git-upload-pack')) {
        // Parse wants/haves from request body
        // Call contract.upload_pack(wants, haves)
        // If response contains packfile-uris, fetch each chunk
        // Return assembled packfile response
    }
    else if (url.pathname.endsWith('/git-receive-pack')) {
        // Read packfile + ref updates from request body
        // If > 1.5MB, split into chunks and call contract multiple times
        // Call contract.receive_pack(pack_data, ref_updates)
        // Return success/failure response
    }
});
```

## Contract storage

Git objects are content-addressed (keyed by SHA hash) and immutable — you only ever insert, never update. This maps naturally to blockchain storage:

| Data | Key | Value | Mutability |
|---|---|---|---|
| Git objects | SHA (20 bytes) | Compressed object bytes | Immutable (write-once) |
| Refs | Refname string | SHA (20 bytes) | Mutable (branch pointers move) |
| Pack chunks | Chunk ID | Packfile bytes | Temporary (can be cleaned up) |

**Storage costs:** NEAR charges ~0.01 NEAR per KB of storage. A typical app repo with a few hundred small JSON entries might use 100-500 KB, costing 1-5 NEAR in storage deposit.

## Rust implementation

The contract uses `gix-pack` and `gix-object` from the [gitoxide](https://github.com/Byron/gitoxide) project for packfile parsing and generation. These are pure Rust and compile to wasm32.

**MVP scope (no delta compression):** Store and transfer raw objects only, skip delta compression. This makes packfiles larger but dramatically simplifies the implementation. For small app repos the size difference is negligible.

### Key dependencies

- `near-sdk` — NEAR smart contract SDK
- `gix-pack` — Packfile parsing and generation
- `gix-object` — Git object parsing (blob, tree, commit)
- `gix-hash` — SHA-1 hashing for object IDs
- `gix-traverse` — Object graph walking for upload-pack

## Multi-tenancy

Each NEAR account can have its own set of repos. The contract can be deployed once and serve multiple users:

```
contract.near/user1/app-data.git
contract.near/user2/app-data.git
```

Access control is native to NEAR — only the repo owner's account can push. Anyone can clone (public repos) or access can be restricted via contract-level checks.

## Future: Git-native governance on-chain

Since the contract controls push acceptance, it can enforce governance rules:

- **Branch protection** — reject direct pushes to `main`, require merges from feature branches
- **Required checks** — run validation scripts (stored in the repo) before accepting a push
- **Policy-as-code** — governance rules are themselves versioned in the repo; changing them requires a reviewed merge
- **Audit trail** — every push is a blockchain transaction, providing cryptographic proof of who changed what and when

## Related projects

- [wasm-git](https://github.com/petersalomonsen/wasm-git) — Git compiled to WebAssembly
- [wasm-git-apps](https://github.com/petersalomonsen/wasm-git-apps) — Devcontainer for AI-driven web app creation using wasm-git
- [gitoxide](https://github.com/Byron/gitoxide) — Pure Rust Git implementation
- [NEAR Protocol](https://near.org) — Layer 1 blockchain with WebAssembly smart contracts

## License

MIT
