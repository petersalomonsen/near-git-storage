use near_sdk::store::{IterableMap, LookupMap};
use near_sdk::{env, near, AccountId, PanicOnDefault, Promise};

/// The factory account that is authorized to create repos as sub-accounts.
/// When the contract is deployed as a sub-account of this factory (via global
/// contract hash), `new()` enforces that only the factory can initialize it.
/// Standalone deployments (not a sub-account) skip this check.
const FACTORY_ACCOUNT: &str = "gitfactory.near";

/// A git object SHA-1 hash as a 40-character hex string.
pub type SHA = String;

/// A transaction hash as a base58-encoded string (NEAR's standard format).
pub type TxHash = String;

/// A git object sent to the contract (borsh-serialized).
/// The client computes the SHA and handles all compression/delta logic.
/// The contract is a trusted key-value store (owner-only writes).
#[near(serializers = [borsh])]
#[derive(Clone)]
pub struct GitObject {
    /// Client-computed git SHA-1 hash for this object.
    pub sha: SHA,
    /// Object type: "blob", "tree", "commit", or "tag"
    pub obj_type: String,
    /// Object data bytes — may be compressed/delta'd by the client.
    /// The contract stores these verbatim and returns them as-is.
    pub data: Vec<u8>,
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

/// A retrieved git object (borsh-serialized in get_objects response).
#[near(serializers = [borsh])]
pub struct RetrievedObject {
    pub obj_type: String,
    /// Stored data bytes (verbatim — client handles decompression)
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

    /// Object data: SHA -> opaque bytes (client-managed compression)
    object_data: LookupMap<SHA, Vec<u8>>,

    /// Repo owner (only owner can push)
    owner: AccountId,
}

#[near]
impl GitStorage {
    #[init]
    pub fn new() -> Self {
        // If deployed as a sub-account of the factory (e.g. myrepo.gitfactory.near),
        // only the factory can call new(). This ensures the factory can charge a
        // service fee for using the global contract.
        // Standalone deployments (not a sub-account of the factory) skip this check.
        let current = env::current_account_id().to_string();
        let is_factory_sub_account = current
            .find('.')
            .map(|i| &current[i + 1..] == FACTORY_ACCOUNT)
            .unwrap_or(false);

        if is_factory_sub_account {
            assert_eq!(
                env::predecessor_account_id().as_str(),
                FACTORY_ACCOUNT,
                "Sub-accounts of the factory can only be initialized by the factory"
            );
        }

        Self {
            refs: IterableMap::new(b"r"),
            object_txs: IterableMap::new(b"o"),
            object_types: LookupMap::new(b"t"),
            object_data: LookupMap::new(b"d"),
            owner: env::signer_account_id(),
        }
    }

    /// Assert that the caller is the contract owner.
    fn assert_owner(&self) {
        assert_eq!(
            env::predecessor_account_id(),
            self.owner,
            "Only the owner can perform this action"
        );
    }

    /// Store git objects (borsh-serialized input/output).
    /// The client provides the SHA and compressed data.
    /// The contract stores them verbatim — no computation.
    #[result_serializer(borsh)]
    pub fn push_objects(
        &mut self,
        #[serializer(borsh)] objects: Vec<GitObject>,
    ) {
        self.assert_owner();

        for obj in &objects {
            // Only store if not already present (objects are immutable)
            if self.object_types.get(&obj.sha).is_none() {
                self.object_types.insert(obj.sha.clone(), obj.obj_type.clone());
                self.object_data.insert(obj.sha.clone(), obj.data.clone());
            }
        }
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
    /// Returns data verbatim — client handles decompression.
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
                        self.object_data.get(&sha).map(|data| RetrievedObject {
                            obj_type: obj_type.clone(),
                            data: data.clone(),
                        })
                    });
                (sha, obj)
            })
            .collect()
    }

    /// Return all stored object SHAs (view call).
    /// Clients use this to determine which objects they're missing,
    /// then fetch only those via get_objects.
    pub fn get_all_shas(&self) -> Vec<SHA> {
        self.object_txs.keys().cloned().collect()
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
