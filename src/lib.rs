use near_sdk::store::{IterableMap, LookupMap};
use near_sdk::{env, near, AccountId, NearToken, PanicOnDefault, Promise};

/// Account that receives the service fee for using the global contract.
/// Set at build time: FEE_RECIPIENT=gitfactory.testnet ./build.sh
const FEE_RECIPIENT: &str = env!("FEE_RECIPIENT");
/// Service fee charged on every new() call to cover global contract deployment costs.
const SERVICE_FEE: NearToken = NearToken::from_millinear(100); // 0.1 NEAR

/// A ref update with compare-and-swap semantics.
#[near(serializers = [borsh, json])]
#[derive(Clone)]
pub struct RefUpdate {
    /// Ref name, e.g. "refs/heads/main"
    pub name: String,
    /// Expected current SHA (None if creating a new ref)
    pub old_sha: Option<String>,
    /// New SHA to set
    pub new_sha: String,
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct GitStorage {
    /// Branch/tag pointers: refname -> SHA
    refs: IterableMap<String, String>,

    /// Stored packfiles, one per push. Index 0, 1, 2, ...
    packs: LookupMap<u32, Vec<u8>>,

    /// Number of stored packfiles.
    pack_count: u32,

    /// Repo owner (only owner can push)
    owner: AccountId,
}

#[near]
impl GitStorage {
    #[init]
    pub fn new() -> Self {
        // Pay service fee to cover global contract deployment costs.
        let fee_recipient: AccountId = FEE_RECIPIENT.parse().unwrap();
        Promise::new(fee_recipient).transfer(SERVICE_FEE);

        Self {
            refs: IterableMap::new(b"R"),
            packs: LookupMap::new(b"P"),
            pack_count: 0,
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

    /// Store a packfile and update refs.
    /// The packfile is stored verbatim — client handles delta compression.
    /// Args are borsh-serialized as sequential fields: pack_data then ref_updates.
    pub fn push(
        &mut self,
        #[serializer(borsh)] pack_data: Vec<u8>,
        #[serializer(borsh)] ref_updates: Vec<RefUpdate>,
    ) {
        self.assert_owner();

        // Store packfile
        if !pack_data.is_empty() {
            self.packs.insert(self.pack_count, pack_data);
            self.pack_count += 1;
        }

        // Update refs with compare-and-swap
        for update in &ref_updates {
            let current = self.refs.get(&update.name).cloned();

            match (&update.old_sha, &current) {
                (None, None) => {
                    self.refs.insert(update.name.clone(), update.new_sha.clone());
                }
                (Some(old), Some(cur)) if old == cur => {
                    self.refs.insert(update.name.clone(), update.new_sha.clone());
                }
                (old_sha, current) => {
                    env::panic_str(&format!(
                        "Ref update CAS failure for '{}': expected {:?}, got {:?}",
                        update.name, old_sha, current
                    ));
                }
            }
        }
    }

    /// Return all refs (view call).
    pub fn get_refs(&self) -> Vec<(String, String)> {
        self.refs.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Return the number of stored packfiles.
    pub fn get_pack_count(&self) -> u32 {
        self.pack_count
    }

    /// Retrieve packfiles starting from `from_index` (borsh input/output, view call).
    /// For full clone: from_index=0. For incremental pull: from_index=last_seen+1.
    #[result_serializer(borsh)]
    pub fn get_packs(
        &self,
        #[serializer(borsh)] from_index: u32,
    ) -> Vec<Vec<u8>> {
        (from_index..self.pack_count)
            .filter_map(|i| self.packs.get(&i).cloned())
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
