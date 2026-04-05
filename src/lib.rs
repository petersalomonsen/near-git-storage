use near_sdk::store::{IterableMap, LookupMap};
use near_sdk::{env, near, AccountId, PanicOnDefault, Promise};
use sha1::{Digest, Sha1};

/// A git object SHA-1 hash as a 40-character hex string.
pub type SHA = String;

/// A transaction hash as a base58-encoded string (NEAR's standard format).
pub type TxHash = String;

/// A git object sent to the contract.
/// The `data` field is base64-encoded raw object content.
#[near(serializers = [json])]
#[derive(Clone)]
pub struct GitObject {
    /// Object type: "blob", "tree", "commit", or "tag"
    pub obj_type: String,
    /// Base64-encoded raw object content
    pub data: String,
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
#[near(serializers = [json])]
pub struct PushObjectsResult {
    pub shas: Vec<SHA>,
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

    /// Object data: SHA -> raw object content bytes
    object_data: LookupMap<SHA, Vec<u8>>,

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

    /// Store git objects. Computes SHA-1 hashes and stores object data
    /// in contract state for later retrieval.
    ///
    /// Returns the computed SHAs for each object.
    pub fn push_objects(&mut self, objects: Vec<GitObject>) -> PushObjectsResult {
        self.assert_owner();

        let mut shas = Vec::with_capacity(objects.len());

        for obj in &objects {
            // Decode base64 data
            let data = base64_decode(&obj.data);

            // Compute git SHA
            let sha = Self::compute_git_sha(&obj.obj_type, &data);

            // Store object data (only if not already present - objects are immutable)
            if self.object_types.get(&sha).is_none() {
                self.object_types.insert(sha.clone(), obj.obj_type.clone());
                self.object_data.insert(sha.clone(), data);
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

    /// Retrieve stored objects by SHA (view call, free).
    /// Returns a list of (sha, obj_type, base64_data) for each found object.
    pub fn get_objects(&self, shas: Vec<SHA>) -> Vec<(SHA, Option<GitObject>)> {
        use base64::Engine;
        shas.into_iter()
            .map(|sha| {
                let obj = self
                    .object_types
                    .get(&sha)
                    .and_then(|obj_type| {
                        self.object_data.get(&sha).map(|data| GitObject {
                            obj_type: obj_type.clone(),
                            data: base64::engine::general_purpose::STANDARD.encode(data.as_slice()),
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

/// Decode base64 string to bytes.
/// Supports standard base64 encoding.
fn base64_decode(input: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .unwrap_or_else(|e| env::panic_str(&format!("Invalid base64: {}", e)))
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
}
