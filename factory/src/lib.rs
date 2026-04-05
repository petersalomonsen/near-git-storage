use near_sdk::{env, near, AccountId, NearToken, PanicOnDefault, Promise};
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::NearSchema;

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
    /// The account that holds the global git-storage contract code
    global_contract: AccountId,
}

#[near]
impl GitFactory {
    #[init]
    pub fn new(global_contract: AccountId) -> Self {
        Self { global_contract }
    }

    /// Create a new git repo as a sub-account of this factory.
    /// The repo name becomes `{repo_name}.{factory_account}`.
    /// The caller becomes the owner of the repo.
    #[payable]
    pub fn create_repo(&mut self, repo_name: String) -> Promise {
        let factory_account = env::current_account_id();
        let sub_account: AccountId = format!("{}.{}", repo_name, factory_account)
            .parse()
            .unwrap_or_else(|_| env::panic_str("Invalid repo name"));

        let deposit = env::attached_deposit();

        assert!(
            deposit >= NearToken::from_millinear(100),
            "Attach at least 0.1 NEAR for account creation and initial storage"
        );

        Promise::new(sub_account)
            .create_account()
            .transfer(deposit)
            .use_global_contract_by_account_id(self.global_contract.clone())
            .function_call(
                "new".to_string(),
                b"{}".to_vec(),
                NearToken::from_near(0),
                near_sdk::Gas::from_tgas(10),
            )
    }

    /// Return the global contract account ID.
    pub fn get_global_contract(&self) -> AccountId {
        self.global_contract.clone()
    }

    /// Web4 handler — serves the create-repo UI.
    pub fn web4_get(&self, #[allow(unused)] request: Web4Request) -> Web4Response {
        Web4Response::Body {
            content_type: "text/html".to_string(),
            body: include_str!("../web4/index.html.base64").to_string(),
        }
    }
}
