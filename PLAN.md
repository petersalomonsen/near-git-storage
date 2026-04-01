# Implementation Plan

## Phase 1: Smart contract — object storage and refs

### 1.1 Project scaffold
- Initialize Rust project with `cargo init`
- Add dependencies: `near-sdk`, `sha1` (for git object hashing)
- Set up `near-workspaces` + `near-sandbox` for integration tests
- Create build script for wasm32 target

### 1.2 Core data structures
```rust
struct GitObject {
    obj_type: String,       // "blob", "tree", "commit", "tag"
    data: Vec<u8>,          // raw object content
}

struct RefUpdate {
    name: String,           // e.g., "refs/heads/main"
    old_sha: Option<SHA>,   // expected current value (for CAS)
    new_sha: SHA,           // new value
}
```

### 1.3 Implement `push_objects`
- Accept `Vec<GitObject>` as function call args
- For each object: compute git SHA (`sha1("blob <size>\0<data>")`)
- Return the computed SHAs
- Do NOT store the object data in contract state
- The data persists in the transaction's function call args

### 1.4 Implement `register_push`
- Accept `tx_hash`, `object_shas`, `ref_updates`
- Store each SHA → tx_hash mapping in `object_txs`
- Validate ref updates (compare-and-swap: old_sha must match current)
- Update refs

### 1.5 Implement view methods
- `get_refs()` — return all refs
- `get_object_locations(shas)` — return SHA → tx_hash mappings

### 1.6 Tests (Phase 1)
- Test push_objects returns correct SHAs
- Test register_push stores mappings and updates refs
- Test ref update CAS (reject stale old_sha)
- Test get_refs and get_object_locations view calls
- Test only owner can push

## Phase 2: Object graph support

### 2.1 Object graph metadata
During `push_objects`, parse minimal structure:
- Commit: extract parent SHA(s) + tree SHA
- Tree: extract child entries (mode, name, SHA)
- Blob/tag: leaf nodes, no children

Store a lightweight graph: `object_children: Map<SHA, Vec<SHA>>`

### 2.2 Implement `resolve_wants`
- Accept `wants` (SHAs client needs) and `haves` (SHAs client already has)
- Walk the object graph from wants, stopping at haves
- Return all reachable SHAs that the client needs
- This is a BFS/DFS over `object_children`

### 2.3 Tests (Phase 2)
- Test graph walking with a simple commit → tree → blob chain
- Test that haves correctly prune the walk
- Test multiple commits (parent chain)

## Phase 3: Service worker — git protocol translation

### 3.1 Pkt-line encoding/decoding
- Implement git pkt-line format (4-hex-digit length prefix)
- Encode refs discovery response
- Decode wants/haves from upload-pack request
- Decode packfile + ref updates from receive-pack request

### 3.2 Packfile parsing (receive-pack)
- Parse incoming packfile header (magic, version, object count)
- Extract individual objects (skip delta objects for MVP)
- Each object: type + size + zlib-compressed data

### 3.3 Packfile generation (upload-pack)
- Build a packfile from individual objects
- Header: "PACK" + version(2) + object count
- Each object: type + size + zlib-compressed data (no deltas for MVP)
- Trailing SHA-1 checksum

### 3.4 NEAR RPC integration
- Fetch refs via view call
- Push: call push_objects, get tx_hash, call register_push
- Fetch: call resolve_wants, get_object_locations, fetch txs from archival RPC
- Extract function call args from transaction response → git object bytes

### 3.5 Service worker fetch handler
- Intercept `*.git/info/refs` → refs discovery
- Intercept `*.git/git-upload-pack` → fetch objects from archival
- Intercept `*.git/git-receive-pack` → push objects to contract

### 3.6 Tests (Phase 3)
- Test pkt-line encoding/decoding
- Test packfile parsing/generation roundtrip
- Test full push flow: packfile → objects → contract → register
- Test full fetch flow: contract → archival RPC → packfile
- Integration test: wasm-git clone → push → clone from another context

## Phase 4: Integration with wasm-git-apps

### 4.1 Add NEAR git server option to wasm-git-apps template
- Service worker with NEAR backend
- Configuration for contract ID and archival RPC endpoint
- Fallback to local git server when NEAR is unavailable

### 4.2 End-to-end Playwright test
- App stores data via wasm-git
- Service worker routes to NEAR sandbox
- Verify data persists across page reloads
- Verify clone from second browser context sees same data

## MVP scope decisions

- **No delta compression** — all objects stored/transferred as full objects
- **No packfile-uris** — single packfile response (fine for small repos)
- **No branch protection** — any ref update accepted (governance is Phase 5+)
- **Single repo per contract** — multi-tenancy is Phase 5+
- **Option B for graph walking** — service worker walks the graph, contract just stores ref → SHA and SHA → tx_hash (simplifies Phase 2, can add contract-side walking later)

## Dependencies

### Smart contract
- `near-sdk` — NEAR smart contract SDK
- `sha1` — SHA-1 hashing for git object IDs

### Service worker
- None (vanilla JS, fetch API for NEAR RPC calls)
- Pako or similar for zlib inflate/deflate (packfile objects are zlib-compressed)

### Testing
- `near-workspaces` — integration test framework
- `near-sandbox` — local NEAR node
- `tokio` — async runtime for tests
