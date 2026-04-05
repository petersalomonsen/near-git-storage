use near_sdk::store::{IterableMap, LookupMap};
use near_sdk::{env, near, AccountId, PanicOnDefault, Promise};
use sha1::{Digest, Sha1};

/// A git object SHA-1 hash as a 40-character hex string.
pub type SHA = String;

/// A transaction hash as a base58-encoded string (NEAR's standard format).
pub type TxHash = String;

/// A git object sent to the contract.
/// Uses borsh serialization — `data` is raw binary, no base64 encoding.
#[near(serializers = [borsh])]
#[derive(Clone)]
pub struct GitObject {
    /// Object type: "blob", "tree", "commit", or "tag"
    pub obj_type: String,
    /// Raw object content bytes
    pub data: Vec<u8>,
    /// Optional SHA of an existing object to delta-compress against.
    /// When provided, the contract computes a binary delta and stores it
    /// instead of the full object if the delta is smaller.
    pub base_sha: Option<SHA>,
}

/// A ref update with compare-and-swap semantics.
#[near(serializers = [json])]
#[derive(Clone)]
pub struct RefUpdate {
    /// Ref name, e.g. "refs/heads/main"
    pub name: String,
    /// Expected current SHA (None if creating a new ref)
    pub old_sha: Option<SHA>,
    /// New SHA to set
    pub new_sha: SHA,
}

/// Result of a push_objects call: the computed SHA for each object.
#[near(serializers = [borsh])]
pub struct PushObjectsResult {
    pub shas: Vec<SHA>,
}

/// A retrieved git object (borsh-serialized in get_objects response).
#[near(serializers = [borsh])]
pub struct RetrievedObject {
    pub obj_type: String,
    /// Full resolved object content bytes (deltas are transparently applied)
    pub data: Vec<u8>,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct GitStorage {
    /// Branch/tag pointers: refname -> SHA
    refs: IterableMap<String, SHA>,

    /// Object locations: SHA -> transaction hash where the data was posted
    object_txs: IterableMap<SHA, TxHash>,

    /// Object types: SHA -> obj_type ("blob", "tree", "commit", "tag")
    object_types: LookupMap<SHA, String>,

    /// Object data: SHA -> raw object content bytes (or delta bytes if delta-compressed)
    object_data: LookupMap<SHA, Vec<u8>>,

    /// Delta base references: SHA -> base SHA (only present for delta-compressed objects)
    delta_base: LookupMap<SHA, SHA>,

    /// Repo owner (only owner can push)
    owner: AccountId,
}

#[near]
impl GitStorage {
    #[init]
    pub fn new() -> Self {
        // Verify that the predecessor is the parent account (the factory).
        // Since repos are sub-accounts (e.g. myrepo.factory.near),
        // the factory is the parent account.
        let current = env::current_account_id().to_string();
        let parent = current
            .find('.')
            .map(|i| &current[i + 1..])
            .unwrap_or_else(|| env::panic_str("Contract must be deployed as a sub-account of the factory"));
        assert_eq!(
            env::predecessor_account_id().as_str(),
            parent,
            "This contract can only be initialized by the factory (parent account)"
        );

        Self {
            refs: IterableMap::new(b"r"),
            object_txs: IterableMap::new(b"o"),
            object_types: LookupMap::new(b"t"),
            object_data: LookupMap::new(b"d"),
            delta_base: LookupMap::new(b"b"),
            owner: env::signer_account_id(),
        }
    }

    /// Compute git SHA-1 for a raw object.
    /// Git object format: "<type> <size>\0<data>"
    fn compute_git_sha(obj_type: &str, data: &[u8]) -> SHA {
        let header = format!("{} {}\0", obj_type, data.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(data);
        let result = hasher.finalize();
        hex::encode(result)
    }

    /// Assert that the caller is the contract owner.
    fn assert_owner(&self) {
        assert_eq!(
            env::predecessor_account_id(),
            self.owner,
            "Only the owner can perform this action"
        );
    }

    /// Resolve the full (uncompressed) data for a stored object,
    /// walking the delta chain if necessary. Returns None if the SHA is unknown.
    fn resolve_data(&self, sha: &SHA) -> Option<Vec<u8>> {
        self.object_data.get(sha)?;

        // Collect the delta chain from sha toward the ultimate base
        let mut deltas: Vec<Vec<u8>> = Vec::new();
        let mut current = sha.clone();

        for _ in 0..256 {
            let data = self.object_data.get(&current)?.clone();
            match self.delta_base.get(&current) {
                Some(next_base) => {
                    deltas.push(data);
                    current = next_base.clone();
                }
                None => {
                    // `current` is a full object — apply deltas in reverse
                    let mut result = data;
                    for delta_data in deltas.into_iter().rev() {
                        result = delta::apply(&result, &delta_data);
                    }
                    return Some(result);
                }
            }
        }

        env::panic_str("Delta chain too deep (>256)")
    }

    /// Store git objects (borsh-serialized input/output).
    ///
    /// When `base_sha` is provided on an object and the base exists,
    /// a binary delta is computed and stored instead of the full object
    /// if the delta is smaller.
    ///
    /// Returns the computed SHAs for each object.
    #[result_serializer(borsh)]
    pub fn push_objects(
        &mut self,
        #[serializer(borsh)] objects: Vec<GitObject>,
    ) -> PushObjectsResult {
        self.assert_owner();

        let mut shas = Vec::with_capacity(objects.len());

        for obj in &objects {
            let sha = Self::compute_git_sha(&obj.obj_type, &obj.data);

            // Store object data (only if not already present - objects are immutable)
            if self.object_types.get(&sha).is_none() {
                self.object_types.insert(sha.clone(), obj.obj_type.clone());

                // Try delta compression if a base SHA is provided
                let mut stored_as_delta = false;
                if let Some(ref base_sha) = obj.base_sha {
                    if let Some(base_data) = self.resolve_data(base_sha) {
                        let delta_data = delta::compute(&base_data, &obj.data);
                        if delta_data.len() < obj.data.len() {
                            self.object_data.insert(sha.clone(), delta_data);
                            self.delta_base.insert(sha.clone(), base_sha.clone());
                            stored_as_delta = true;
                        }
                    }
                }

                if !stored_as_delta {
                    self.object_data.insert(sha.clone(), obj.data.clone());
                }
            }

            shas.push(sha);
        }

        PushObjectsResult { shas }
    }

    /// Register a previous push_objects transaction and update refs.
    /// Called after push_objects, with the tx_hash from that transaction.
    ///
    /// - Stores SHA -> tx_hash mappings for each object
    /// - Updates refs with compare-and-swap semantics
    pub fn register_push(
        &mut self,
        tx_hash: TxHash,
        object_shas: Vec<SHA>,
        ref_updates: Vec<RefUpdate>,
    ) {
        self.assert_owner();

        // Store SHA -> tx_hash mappings
        for sha in &object_shas {
            self.object_txs.insert(sha.clone(), tx_hash.clone());
        }

        // Update refs with compare-and-swap
        for update in &ref_updates {
            let current = self.refs.get(&update.name).cloned();

            match (&update.old_sha, &current) {
                // Creating a new ref: old_sha is None, current must also be None
                (None, None) => {
                    self.refs.insert(update.name.clone(), update.new_sha.clone());
                }
                // Updating an existing ref: old_sha must match current
                (Some(old), Some(cur)) if old == cur => {
                    self.refs.insert(update.name.clone(), update.new_sha.clone());
                }
                // Mismatch: CAS failure
                (old_sha, current) => {
                    env::panic_str(&format!(
                        "Ref update CAS failure for '{}': expected {:?}, got {:?}",
                        update.name, old_sha, current
                    ));
                }
            }
        }
    }

    /// Return all refs (view call, free).
    pub fn get_refs(&self) -> Vec<(String, SHA)> {
        self.refs.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Return transaction locations for requested objects (view call, free).
    pub fn get_object_locations(&self, shas: Vec<SHA>) -> Vec<(SHA, Option<TxHash>)> {
        shas.into_iter()
            .map(|sha| {
                let tx = self.object_txs.get(&sha).cloned();
                (sha, tx)
            })
            .collect()
    }

    /// Retrieve stored objects by SHA (borsh-serialized input/output, view call).
    /// Delta-compressed objects are automatically resolved to full content.
    #[result_serializer(borsh)]
    pub fn get_objects(
        &self,
        #[serializer(borsh)] shas: Vec<SHA>,
    ) -> Vec<(SHA, Option<RetrievedObject>)> {
        shas.into_iter()
            .map(|sha| {
                let obj = self
                    .object_types
                    .get(&sha)
                    .and_then(|obj_type| {
                        self.resolve_data(&sha).map(|data| RetrievedObject {
                            obj_type: obj_type.clone(),
                            data,
                        })
                    });
                (sha, obj)
            })
            .collect()
    }

    /// Return the contract owner.
    pub fn get_owner(&self) -> AccountId {
        self.owner.clone()
    }

    /// Delete this repo contract and send remaining funds to the owner.
    /// Can only be called by the owner.
    pub fn self_delete(&mut self) -> Promise {
        self.assert_owner();
        Promise::new(env::current_account_id()).delete_account(self.owner.clone())
    }
}

/// Inline hex encoding to avoid adding another dependency.
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes
            .as_ref()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }
}

/// Git-compatible binary delta encoding/decoding.
///
/// Delta format (matches git's pack delta format):
/// - Header: base_size (varint), target_size (varint)
/// - Instructions:
///   - COPY (high bit set): copy bytes from base at offset+length
///   - INSERT (high bit clear, 1-127): literal bytes follow
mod delta {
    use std::collections::HashMap;

    const BLOCK_SIZE: usize = 16;

    fn encode_size(buf: &mut Vec<u8>, mut val: usize) {
        loop {
            let mut byte = (val & 0x7f) as u8;
            val >>= 7;
            if val > 0 {
                byte |= 0x80;
            }
            buf.push(byte);
            if val == 0 {
                break;
            }
        }
    }

    fn decode_size(data: &[u8], pos: &mut usize) -> usize {
        let mut val = 0usize;
        let mut shift = 0;
        loop {
            let byte = data[*pos];
            *pos += 1;
            val |= ((byte & 0x7f) as usize) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        val
    }

    /// FNV-1a hash for block matching.
    fn fnv_hash(data: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for &b in data {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }

    fn flush_inserts(delta: &mut Vec<u8>, buf: &mut Vec<u8>) {
        while !buf.is_empty() {
            let count = buf.len().min(127);
            delta.push(count as u8);
            delta.extend_from_slice(&buf[..count]);
            buf.drain(..count);
        }
    }

    fn emit_copy(delta: &mut Vec<u8>, offset: usize, size: usize) {
        let mut cmd: u8 = 0x80;
        let mut data = Vec::new();

        for i in 0..4 {
            let byte = ((offset >> (i * 8)) & 0xff) as u8;
            if byte != 0 {
                cmd |= 1 << i;
                data.push(byte);
            }
        }

        let enc_size = if size == 0x10000 { 0 } else { size };
        for i in 0..3 {
            let byte = ((enc_size >> (i * 8)) & 0xff) as u8;
            if byte != 0 {
                cmd |= 1 << (4 + i);
                data.push(byte);
            }
        }

        delta.push(cmd);
        delta.extend_from_slice(&data);
    }

    /// Compute a binary delta from `base` to `target`.
    pub fn compute(base: &[u8], target: &[u8]) -> Vec<u8> {
        // Build index of BLOCK_SIZE-byte windows in base
        let mut index: HashMap<u64, Vec<usize>> = HashMap::new();
        if base.len() >= BLOCK_SIZE {
            for i in (0..=base.len() - BLOCK_SIZE).step_by(BLOCK_SIZE) {
                let hash = fnv_hash(&base[i..i + BLOCK_SIZE]);
                index.entry(hash).or_default().push(i);
            }
        }

        let mut result = Vec::new();
        encode_size(&mut result, base.len());
        encode_size(&mut result, target.len());

        let mut pos = 0;
        let mut insert_buf: Vec<u8> = Vec::new();

        while pos < target.len() {
            let remaining = target.len() - pos;
            let mut best_offset = 0usize;
            let mut best_len = 0usize;

            if remaining >= BLOCK_SIZE {
                let hash = fnv_hash(&target[pos..pos + BLOCK_SIZE]);
                if let Some(offsets) = index.get(&hash) {
                    for &base_off in offsets {
                        let max_len = remaining.min(base.len() - base_off);
                        let mut len = 0;
                        while len < max_len && target[pos + len] == base[base_off + len] {
                            len += 1;
                        }
                        if len > best_len {
                            best_len = len;
                            best_offset = base_off;
                        }
                    }
                }
            }

            if best_len >= BLOCK_SIZE {
                flush_inserts(&mut result, &mut insert_buf);
                // Emit COPY instructions (max 0x10000 bytes each)
                let mut copied = 0;
                while copied < best_len {
                    let chunk = (best_len - copied).min(0x10000);
                    emit_copy(&mut result, best_offset + copied, chunk);
                    copied += chunk;
                }
                pos += best_len;
            } else {
                insert_buf.push(target[pos]);
                pos += 1;
            }
        }

        flush_inserts(&mut result, &mut insert_buf);
        result
    }

    /// Apply a delta to reconstruct the target from base.
    pub fn apply(base: &[u8], delta_data: &[u8]) -> Vec<u8> {
        let mut pos = 0;
        let base_size = decode_size(delta_data, &mut pos);
        let target_size = decode_size(delta_data, &mut pos);

        assert_eq!(base.len(), base_size, "Delta base size mismatch");

        let mut target = Vec::with_capacity(target_size);

        while pos < delta_data.len() {
            let cmd = delta_data[pos];
            pos += 1;

            if cmd & 0x80 != 0 {
                // COPY instruction
                let mut offset = 0usize;
                for i in 0..4 {
                    if cmd & (1 << i) != 0 {
                        offset |= (delta_data[pos] as usize) << (i * 8);
                        pos += 1;
                    }
                }

                let mut size = 0usize;
                for i in 0..3 {
                    if cmd & (1 << (4 + i)) != 0 {
                        size |= (delta_data[pos] as usize) << (i * 8);
                        pos += 1;
                    }
                }

                if size == 0 {
                    size = 0x10000;
                }

                target.extend_from_slice(&base[offset..offset + size]);
            } else if cmd > 0 {
                // INSERT instruction
                let count = cmd as usize;
                target.extend_from_slice(&delta_data[pos..pos + count]);
                pos += count;
            } else {
                panic!("Invalid delta instruction: 0");
            }
        }

        assert_eq!(target.len(), target_size, "Delta target size mismatch");
        target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compute git SHA outside the contract for test verification.
    fn git_sha(obj_type: &str, data: &[u8]) -> String {
        let header = format!("{} {}\0", obj_type, data.len());
        let mut hasher = Sha1::new();
        hasher.update(header.as_bytes());
        hasher.update(data);
        hex::encode(hasher.finalize())
    }

    #[test]
    fn test_compute_git_sha_blob() {
        // "hello world" as a git blob should produce a well-known SHA
        // git hash-object -t blob --stdin <<< "hello world" (without trailing newline)
        let data = b"hello world";
        let sha = git_sha("blob", data);
        // Known git SHA for blob "hello world" (no newline)
        assert_eq!(sha, "95d09f2b10159347eece71399a7e2e907ea3df4f");
    }

    #[test]
    fn test_compute_git_sha_blob_with_newline() {
        // git hash-object -t blob --stdin <<< "hello world" (with trailing newline)
        let data = b"hello world\n";
        let sha = git_sha("blob", data);
        assert_eq!(sha, "3b18e512dba79e4c8300dd08aeb37f8e728b8dad");
    }

    #[test]
    fn test_delta_roundtrip_identical() {
        let base = b"hello world, this is a test of delta compression!";
        let target = b"hello world, this is a test of delta compression!";
        let d = delta::compute(base, target);
        let result = delta::apply(base, &d);
        assert_eq!(result, target);
    }

    #[test]
    fn test_delta_roundtrip_small_change() {
        // Base and target must be >= BLOCK_SIZE (16) for copy matching
        let base = b"The quick brown fox jumps over the lazy dog. The end.";
        let target = b"The quick brown cat jumps over the lazy dog. The end.";
        let d = delta::compute(base, target);
        let result = delta::apply(base, &d);
        assert_eq!(result, target);
        // Delta should be smaller than full target
        assert!(d.len() < target.len(), "delta {} >= target {}", d.len(), target.len());
    }

    #[test]
    fn test_delta_roundtrip_large_similar() {
        // Simulate a source file with a small edit
        let base: Vec<u8> = (0..4096).map(|i| b"abcdefghijklmnop"[i % 16]).collect();
        let mut target = base.clone();
        // Change a few bytes in the middle
        target[2048] = b'X';
        target[2049] = b'Y';
        target[2050] = b'Z';

        let d = delta::compute(&base, &target);
        let result = delta::apply(&base, &d);
        assert_eq!(result, target);
        // Should be much smaller
        assert!(d.len() < target.len() / 2, "delta {} not much smaller than target {}", d.len(), target.len());
    }

    #[test]
    fn test_delta_roundtrip_completely_different() {
        let base = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let target = b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let d = delta::compute(base, target);
        let result = delta::apply(base, &d);
        assert_eq!(result, target);
    }

    #[test]
    fn test_delta_roundtrip_empty_target() {
        let base = b"some base data here with enough length";
        let target = b"";
        let d = delta::compute(base, target);
        let result = delta::apply(base, &d);
        assert_eq!(result, target);
    }

    #[test]
    fn test_delta_roundtrip_empty_base() {
        let base = b"";
        let target = b"all new data that did not exist before!";
        let d = delta::compute(base, target);
        let result = delta::apply(base, &d);
        assert_eq!(result, target);
    }

    #[test]
    fn test_delta_compression_ratio_realistic() {
        // Simulate two versions of a source file (like synth.ts)
        let mut base = Vec::new();
        for i in 0..1000 {
            base.extend_from_slice(format!("line {}: some code here that is typical\n", i).as_bytes());
        }
        // Target: same but with 5 lines changed
        let mut target = base.clone();
        for line_num in [100, 300, 500, 700, 900] {
            let needle = format!("line {}: ", line_num);
            let offset = target.windows(needle.len())
                .position(|w| w == needle.as_bytes())
                .unwrap();
            let end = target[offset..].iter().position(|&b| b == b'\n').unwrap() + offset + 1;
            let new_line = format!("line {}: MODIFIED code with different content\n", line_num);
            target.splice(offset..end, new_line.bytes());
        }

        let d = delta::compute(&base, &target);
        let result = delta::apply(&base, &d);
        assert_eq!(result, target);

        let ratio = base.len() as f64 / d.len() as f64;
        assert!(ratio > 3.0, "Expected >3x compression, got {:.1}x (delta={}, base={})", ratio, d.len(), base.len());
    }
}
