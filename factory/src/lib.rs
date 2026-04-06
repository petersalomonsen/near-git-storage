use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::{env, near, AccountId, NearSchema, NearToken, PanicOnDefault, Promise};

/// Fee charged by the factory for using the global contract.
const SERVICE_FEE: NearToken = NearToken::from_millinear(100); // 0.1 NEAR

#[derive(Debug, Serialize, Deserialize, NearSchema)]
#[serde(crate = "near_sdk::serde")]
pub struct Web4Request {
    pub path: String,
    #[serde(default)]
    pub params: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub query: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, NearSchema)]
#[serde(crate = "near_sdk::serde", untagged)]
pub enum Web4Response {
    Body {
        #[serde(rename = "contentType")]
        content_type: String,
        body: String,
    },
}

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct GitFactory {
    /// SHA-256 hash of the global git-storage contract code (32 bytes).
    /// Repos are pinned to this exact version.
    global_contract_hash: Vec<u8>,
    /// Factory owner who can update the global contract hash.
    owner: AccountId,
}

#[near]
impl GitFactory {
    #[init]
    pub fn new(global_contract_hash: String) -> Self {
        let hash = Self::decode_hex(&global_contract_hash);
        assert_eq!(hash.len(), 32, "global_contract_hash must be 32 bytes (64 hex chars)");
        Self {
            global_contract_hash: hash,
            owner: env::predecessor_account_id(),
        }
    }

    /// Create a new git repo as a sub-account of this factory.
    /// The repo name becomes `{repo_name}.{factory_account}`.
    /// The caller becomes the owner of the repo.
    ///
    /// A service fee of 0.1 NEAR is deducted; the rest funds the repo account.
    #[payable]
    pub fn create_repo(&mut self, repo_name: String) -> Promise {
        let factory_account = env::current_account_id();
        let sub_account: AccountId = format!("{}.{}", repo_name, factory_account)
            .parse()
            .unwrap_or_else(|_| env::panic_str("Invalid repo name"));

        let deposit = env::attached_deposit();
        assert!(
            deposit > SERVICE_FEE,
            "Attach more than 0.1 NEAR (0.1 service fee + storage funding)"
        );

        let repo_funding = deposit.saturating_sub(SERVICE_FEE);

        Promise::new(sub_account)
            .create_account()
            .transfer(repo_funding)
            .use_global_contract(self.global_contract_hash.clone())
            .function_call(
                "new".to_string(),
                b"{}".to_vec(),
                NearToken::from_near(0),
                near_sdk::Gas::from_tgas(10),
            )
    }

    /// Update the global contract hash. Only the factory owner can call this.
    /// New repos will use this hash; existing repos stay on their original version.
    pub fn set_global_contract_hash(&mut self, global_contract_hash: String) {
        assert_eq!(
            env::predecessor_account_id(),
            self.owner,
            "Only the factory owner can update the global contract hash"
        );
        let hash = Self::decode_hex(&global_contract_hash);
        assert_eq!(hash.len(), 32, "global_contract_hash must be 32 bytes (64 hex chars)");
        self.global_contract_hash = hash;
    }

    /// Return the current global contract hash as hex.
    pub fn get_global_contract_hash(&self) -> String {
        self.global_contract_hash
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    /// Return the factory owner.
    pub fn get_owner(&self) -> AccountId {
        self.owner.clone()
    }

    /// Web4 handler — serves the create-repo UI.
    pub fn web4_get(&self, #[allow(unused)] request: Web4Request) -> Web4Response {
        Web4Response::Body {
            content_type: "text/html".to_string(),
            body: include_str!("../web4/index.html.base64").to_string(),
        }
    }

    fn decode_hex(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16)
                .unwrap_or_else(|_| env::panic_str("Invalid hex in hash")))
            .collect()
    }
}
