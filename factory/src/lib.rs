use near_sdk::{env, near, AccountId, NearToken, PanicOnDefault, Promise};

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
            deposit >= NearToken::from_millinear(500),
            "Attach at least 0.5 NEAR for storage"
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
}
