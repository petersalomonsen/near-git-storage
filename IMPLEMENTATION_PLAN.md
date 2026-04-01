# Implementation Plan

This document outlines the step-by-step implementation plan for the near-git-storage smart contract.

## Prerequisites

- Rust toolchain (stable)
- [cargo-near](https://github.com/near/cargo-near) for building and deploying NEAR smart contracts
- NEAR CLI for account management and testing

## Phase 1: Project Setup

### 1.1 Initialize the smart contract project

```bash
cargo near new near-git-storage
cd near-git-storage
```

This creates a new NEAR smart contract project with the proper structure and dependencies.

### 1.2 Add Git-related dependencies

Update `Cargo.toml` to include gitoxide crates:

```toml
[dependencies]
near-sdk = "5.6"
gix-pack = { version = "0.53", default-features = false }
gix-object = { version = "0.46", default-features = false }
gix-hash = { version = "0.14", default-features = false }
gix-traverse = { version = "0.42", default-features = false }
```

Note: Versions may need adjustment. Some features may need to be disabled for wasm32 compatibility.

### 1.3 Verify wasm32 compilation

```bash
cargo near build
```

Resolve any compilation issues with gitoxide crates on wasm32-unknown-unknown target.

---

## Phase 2: Storage and Data Model

### 2.1 Define core data structures

```rust
use near_sdk::borsh::{BorshDeserialize, BorshSerialize};
use near_sdk::store::{LookupMap, UnorderedMap};
use near_sdk::{near, AccountId, NearToken, env};

type SHA = [u8; 20];

#[near(contract_state)]
pub struct Contract {
    /// Git objects keyed by SHA hash (per user)
    /// Key: (owner_account_id, sha) -> compressed object bytes
    objects: LookupMap<(AccountId, SHA), Vec<u8>>,
    
    /// Refs keyed by name (per user)
    /// Key: (owner_account_id, ref_name) -> SHA
    refs: UnorderedMap<(AccountId, String), SHA>,
    
    /// Temporary pack chunks for large transfers
    pack_chunks: LookupMap<(AccountId, String), Vec<u8>>,
    
    /// Storage balance per user (prepaid storage credits)
    storage_balances: LookupMap<AccountId, NearToken>,
}
```

### 2.2 Implement storage purchase mechanism

Each user must purchase storage before storing data. Storage credits are deducted based on actual usage.

```rust
#[near]
impl Contract {
    /// Users call this with attached NEAR to purchase storage credits
    #[payable]
    pub fn storage_deposit(&mut self) {
        let account_id = env::predecessor_account_id();
        let deposit = env::attached_deposit();
        
        let current_balance = self.storage_balances
            .get(&account_id)
            .unwrap_or(NearToken::from_yoctonear(0));
        
        let new_balance = current_balance.saturating_add(deposit);
        self.storage_balances.insert(account_id, new_balance);
    }
    
    /// View method to check storage balance
    pub fn storage_balance_of(&self, account_id: AccountId) -> NearToken {
        self.storage_balances
            .get(&account_id)
            .unwrap_or(NearToken::from_yoctonear(0))
    }
    
    /// Withdraw unused storage credits
    pub fn storage_withdraw(&mut self, amount: Option<NearToken>) -> NearToken {
        let account_id = env::predecessor_account_id();
        let balance = self.storage_balances
            .get(&account_id)
            .unwrap_or(NearToken::from_yoctonear(0));
        
        let withdraw_amount = amount.unwrap_or(balance);
        assert!(withdraw_amount <= balance, "Insufficient storage balance");
        
        let new_balance = balance.saturating_sub(withdraw_amount);
        self.storage_balances.insert(account_id.clone(), new_balance);
        
        // Transfer NEAR back to user
        Promise::new(account_id).transfer(withdraw_amount);
        
        withdraw_amount
    }
}
```

### 2.3 Implement storage cost deduction

Storage costs are calculated by measuring `env::storage_usage()` before and after storing data:

```rust
impl Contract {
    /// Internal helper to charge storage costs
    fn charge_storage(&mut self, account_id: &AccountId, storage_before: u64) {
        let storage_after = env::storage_usage();
        
        if storage_after > storage_before {
            let storage_used = storage_after - storage_before;
            // NEAR charges ~10^19 yoctoNEAR per byte (0.00001 NEAR per byte)
            let storage_cost = NearToken::from_yoctonear(
                storage_used as u128 * env::storage_byte_cost().as_yoctonear()
            );
            
            let balance = self.storage_balances
                .get(account_id)
                .unwrap_or(NearToken::from_yoctonear(0));
            
            assert!(
                balance >= storage_cost,
                "Insufficient storage balance. Need {} but have {}",
                storage_cost,
                balance
            );
            
            let new_balance = balance.saturating_sub(storage_cost);
            self.storage_balances.insert(account_id.clone(), new_balance);
        }
    }
}
```

---

## Phase 3: Core Git Protocol Methods

### 3.1 Implement `get_refs`

```rust
#[near]
impl Contract {
    /// Return all refs for a given repository owner
    pub fn get_refs(&self, owner: AccountId) -> Vec<(String, String)> {
        self.refs
            .iter()
            .filter(|((account, _), _)| account == &owner)
            .map(|((_, ref_name), sha)| {
                (ref_name.clone(), hex::encode(sha))
            })
            .collect()
    }
}
```

### 3.2 Implement `receive_pack` (push)

```rust
#[near]
impl Contract {
    /// Handle push - unpack objects and update refs
    /// Measures and charges storage automatically
    pub fn receive_pack(
        &mut self,
        pack_data: Vec<u8>,
        ref_updates: Vec<RefUpdate>,
    ) {
        let account_id = env::predecessor_account_id();
        let storage_before = env::storage_usage();
        
        // Parse packfile and extract objects
        let objects = self.parse_packfile(&pack_data);
        
        // Store each object
        for (sha, data) in objects {
            let key = (account_id.clone(), sha);
            // Only insert if not already present (objects are immutable)
            if !self.objects.contains_key(&key) {
                self.objects.insert(key, data);
            }
        }
        
        // Apply ref updates
        for update in ref_updates {
            self.update_ref(&account_id, update);
        }
        
        // Charge for storage used
        self.charge_storage(&account_id, storage_before);
    }
}
```

### 3.3 Implement `upload_pack` (fetch/clone)

```rust
#[near]
impl Contract {
    /// Handle fetch/clone - return packfile of requested objects
    pub fn upload_pack(
        &self,
        owner: AccountId,
        wants: Vec<String>,  // SHA hexes the client wants
        haves: Vec<String>,  // SHA hexes the client already has
    ) -> UploadPackResponse {
        // Convert hex strings to SHA bytes
        let wants: Vec<SHA> = wants.iter()
            .map(|h| hex_to_sha(h))
            .collect();
        let haves: Vec<SHA> = haves.iter()
            .map(|h| hex_to_sha(h))
            .collect();
        
        // Walk object graph to find needed objects
        let needed_objects = self.walk_objects(&owner, &wants, &haves);
        
        // Build packfile
        let packfile = self.build_packfile(&owner, &needed_objects);
        
        // If packfile fits in response, return directly
        // Otherwise, store chunks and return URIs
        if packfile.len() <= MAX_RESPONSE_SIZE {
            UploadPackResponse::Packfile(packfile)
        } else {
            let uris = self.chunk_and_store_packfile(&owner, packfile);
            UploadPackResponse::PackfileUris(uris)
        }
    }
}
```

---

## Phase 4: Packfile Parsing and Generation

### 4.1 Implement packfile parser (MVP - no deltas)

```rust
impl Contract {
    fn parse_packfile(&self, pack_data: &[u8]) -> Vec<(SHA, Vec<u8>)> {
        // Use gix-pack to parse the packfile
        // For MVP: reject packs with delta objects, require --no-thin
        // Return list of (sha, compressed_object_data) pairs
        todo!("Implement using gix-pack")
    }
}
```

### 4.2 Implement packfile generator (MVP - no deltas)

```rust
impl Contract {
    fn build_packfile(&self, owner: &AccountId, shas: &[SHA]) -> Vec<u8> {
        // Build a packfile containing the requested objects
        // For MVP: store as raw objects, no delta compression
        todo!("Implement using gix-pack")
    }
}
```

### 4.3 Implement object graph walking

```rust
impl Contract {
    fn walk_objects(
        &self,
        owner: &AccountId,
        wants: &[SHA],
        haves: &[SHA],
    ) -> Vec<SHA> {
        // Starting from 'wants', traverse commit -> tree -> blob graph
        // Stop at 'haves' (objects the client already has)
        // Return all objects needed to satisfy the fetch
        todo!("Implement using gix-traverse")
    }
}
```

---

## Phase 5: Large Transfer Support

### 5.1 Chunked push for large repositories

```rust
#[near]
impl Contract {
    /// Start a chunked push session
    pub fn push_chunk_start(&mut self) -> String {
        let account_id = env::predecessor_account_id();
        let session_id = format!("{}-{}", account_id, env::block_timestamp());
        // Initialize empty buffer for this session
        self.pack_chunks.insert(
            (account_id, session_id.clone()),
            Vec::new()
        );
        session_id
    }
    
    /// Append data to a chunked push session
    pub fn push_chunk_append(&mut self, session_id: String, data: Vec<u8>) {
        let account_id = env::predecessor_account_id();
        let key = (account_id, session_id);
        let mut buffer = self.pack_chunks.get(&key)
            .expect("Session not found");
        buffer.extend(data);
        self.pack_chunks.insert(key, buffer);
    }
    
    /// Finalize chunked push
    pub fn push_chunk_finalize(
        &mut self,
        session_id: String,
        ref_updates: Vec<RefUpdate>,
    ) {
        let account_id = env::predecessor_account_id();
        let key = (account_id.clone(), session_id);
        let pack_data = self.pack_chunks.remove(&key)
            .expect("Session not found");
        
        // Delegate to regular receive_pack
        self.receive_pack(pack_data, ref_updates);
    }
}
```

### 5.2 Chunked fetch responses (packfile-uris)

```rust
impl Contract {
    fn chunk_and_store_packfile(
        &self,
        owner: &AccountId,
        packfile: Vec<u8>,
    ) -> Vec<(String, String)> {
        // Split packfile into chunks under MAX_RESPONSE_SIZE
        // Store each chunk with a unique ID
        // Return list of (chunk_sha, chunk_uri) pairs
        todo!()
    }
    
    /// View method to retrieve a packfile chunk
    pub fn get_pack_chunk(&self, owner: AccountId, chunk_id: String) -> Vec<u8> {
        self.pack_chunks
            .get(&(owner, chunk_id))
            .expect("Chunk not found")
    }
}
```

---

## Phase 6: Service Worker Implementation

### 6.1 Create service worker scaffold

```javascript
// sw.js
const NEAR_RPC = 'https://rpc.mainnet.near.org';
const CONTRACT_ID = 'git-storage.near';

self.addEventListener('fetch', async (event) => {
    const url = new URL(event.request.url);
    
    // Only intercept .git URLs
    if (!url.pathname.includes('.git/')) return;
    
    event.respondWith(handleGitRequest(event.request, url));
});
```

### 6.2 Implement info/refs handler

```javascript
async function handleInfoRefs(url, service) {
    const owner = extractOwner(url);
    const refs = await callViewMethod('get_refs', { owner });
    
    // Format as git pkt-line protocol
    const response = formatPktLineRefs(refs, service);
    return new Response(response, {
        headers: { 'Content-Type': `application/x-${service}-advertisement` }
    });
}
```

### 6.3 Implement git-upload-pack handler (fetch)

```javascript
async function handleUploadPack(request, url) {
    const owner = extractOwner(url);
    const body = await request.arrayBuffer();
    const { wants, haves } = parseUploadPackRequest(body);
    
    const result = await callViewMethod('upload_pack', { owner, wants, haves });
    
    if (result.Packfile) {
        return new Response(formatPackfileResponse(result.Packfile));
    } else {
        // Fetch chunks and assemble
        const chunks = await Promise.all(
            result.PackfileUris.map(([sha, uri]) => 
                callViewMethod('get_pack_chunk', { owner, chunk_id: sha })
            )
        );
        return new Response(formatPackfileResponse(assembleChunks(chunks)));
    }
}
```

### 6.4 Implement git-receive-pack handler (push)

```javascript
async function handleReceivePack(request, url) {
    const owner = extractOwner(url);
    const body = await request.arrayBuffer();
    const { packData, refUpdates } = parseReceivePackRequest(body);
    
    // Check if we need to chunk
    if (packData.length > MAX_TX_SIZE) {
        await chunkedPush(owner, packData, refUpdates);
    } else {
        await callChangeMethod('receive_pack', {
            pack_data: Array.from(packData),
            ref_updates: refUpdates
        });
    }
    
    return new Response(formatReceivePackResponse('ok'));
}
```

---

## Phase 7: Testing

### 7.1 Unit tests for packfile parsing

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_parse_simple_packfile() {
        // Create a packfile with known objects
        // Verify parsing extracts correct SHAs and data
    }
    
    #[test]
    fn test_build_packfile() {
        // Store some objects
        // Build a packfile
        // Verify it can be parsed back
    }
}
```

### 7.2 Integration tests with sandbox

```rust
#[tokio::test]
async fn test_push_and_fetch() {
    let sandbox = near_workspaces::sandbox().await.unwrap();
    let contract = sandbox.dev_deploy(CONTRACT_WASM).await.unwrap();
    
    // User deposits storage
    // User pushes a simple repo
    // User fetches it back
    // Verify refs and objects match
}
```

### 7.3 End-to-end test with wasm-git

Create a test page that:
1. Initializes wasm-git
2. Creates a repo and makes commits
3. Pushes to the contract via service worker
4. Clones from the contract
5. Verifies content matches

---

## Phase 8: Deployment

### 8.1 Build for production

```bash
cargo near build --release
```

### 8.2 Deploy to testnet

```bash
cargo near deploy --account-id git-storage.testnet
```

### 8.3 Deploy to mainnet

```bash
cargo near deploy --account-id git-storage.near
```

---

## Milestones Summary

| Phase | Description | Deliverable |
|-------|-------------|-------------|
| 1 | Project Setup | Compiling empty contract with gitoxide deps |
| 2 | Storage Model | Storage deposit/withdraw, data structures |
| 3 | Core Methods | `get_refs`, `receive_pack`, `upload_pack` stubs |
| 4 | Packfile Logic | Parse and generate packfiles (MVP, no deltas) |
| 5 | Large Transfers | Chunked push/fetch for repos > 1.5MB |
| 6 | Service Worker | Browser integration with wasm-git |
| 7 | Testing | Unit, integration, and e2e tests passing |
| 8 | Deployment | Live on testnet, then mainnet |

---

## Storage Economics Summary

| Action | Storage Cost |
|--------|--------------|
| Deposit | User attaches NEAR to `storage_deposit()` |
| Push | Storage measured before/after, cost deducted from balance |
| Fetch | Free (view calls have no gas cost) |
| Withdraw | User can reclaim unused balance via `storage_withdraw()` |

**Cost estimation:** ~0.00001 NEAR per byte (~10 NEAR per MB). A typical small app repo (100KB) costs ~1 NEAR in storage.
