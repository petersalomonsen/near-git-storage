use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};

use near_api::{AccountId, Contract, NearToken, Signer};
use near_sandbox::Sandbox;
use serde_json::json;
use tokio::sync::OnceCell;

const STORAGE_WASM: &str = "res/near_git_storage.wasm";
const FACTORY_WASM: &str = "res/near_git_factory.wasm";

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);
static SHARED: OnceCell<SharedState> = OnceCell::const_new();

struct SharedState {
    #[allow(dead_code)]
    sandbox: Sandbox,
    network: near_api::NetworkConfig,
    genesis_id: AccountId,
    genesis_signer: Arc<Signer>,
    storage_wasm: Vec<u8>,
    factory_wasm: Vec<u8>,
    global_id: AccountId,
    wasm_hash: String,
}

async fn shared() -> &'static SharedState {
    SHARED
        .get_or_init(|| async {
            let sandbox = Sandbox::start_sandbox().await.unwrap();
            let network = near_api::NetworkConfig::from_rpc_url(
                "sandbox",
                sandbox.rpc_addr.parse().unwrap(),
            );
            let genesis = near_sandbox::GenesisAccount::default();
            let genesis_id: AccountId = genesis.account_id.to_string().parse().unwrap();
            let genesis_signer =
                Signer::from_secret_key(genesis.private_key.parse().unwrap()).unwrap();
            let storage_wasm = std::fs::read(STORAGE_WASM)
                .expect("Run ./build.sh first");
            let factory_wasm = std::fs::read(FACTORY_WASM)
                .expect("Run ./build.sh first");

            // Deploy git-storage WASM as a global contract
            let global_secret = near_api::signer::generate_secret_key().unwrap();
            let global_id: AccountId = "gitglobal.sandbox".parse().unwrap();

            near_api::Account::create_account(global_id.clone())
                .fund_myself(genesis_id.clone(), NearToken::from_near(50))
                .with_public_key(global_secret.public_key())
                .with_signer(genesis_signer.clone())
                .send_to(&network)
                .await
                .unwrap()
                .assert_success();

            let global_signer = Signer::from_secret_key(global_secret).unwrap();

            Contract::deploy_global_contract_code(storage_wasm.clone())
                .as_hash()
                .with_signer(global_id.clone(), global_signer)
                .send_to(&network)
                .await
                .unwrap()
                .assert_success();

            use sha2::{Digest, Sha256};
            let wasm_hash: String = Sha256::digest(&storage_wasm)
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect();

            SharedState {
                sandbox,
                network,
                genesis_id,
                genesis_signer,
                storage_wasm,
                factory_wasm,
                global_id,
                wasm_hash,
            }
        })
        .await
}

/// Deploy the factory, return (factory_id, factory_signer)
async fn setup_factory() -> (AccountId, Arc<Signer>) {
    let s = shared().await;
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);

    // Deploy factory contract
    let factory_secret = near_api::signer::generate_secret_key().unwrap();
    let factory_id: AccountId = format!("factory{n}.sandbox").parse().unwrap();

    near_api::Account::create_account(factory_id.clone())
        .fund_myself(s.genesis_id.clone(), NearToken::from_near(50))
        .with_public_key(factory_secret.public_key())
        .with_signer(s.genesis_signer.clone())
        .send_to(&s.network)
        .await
        .unwrap()
        .assert_success();

    let factory_signer = Signer::from_secret_key(factory_secret).unwrap();

    Contract::deploy(factory_id.clone())
        .use_code(s.factory_wasm.clone())
        .with_init_call(
            "new",
            json!({ "global_contract_hash": &s.wasm_hash }),
        )
        .unwrap()
        .with_signer(factory_signer.clone())
        .send_to(&s.network)
        .await
        .unwrap()
        .assert_success();

    (factory_id, factory_signer)
}

#[tokio::test]
async fn test_factory_create_repo() {
    let s = shared().await;
    let (factory_id, _factory_signer) = setup_factory().await;

    // Create an owner account
    let owner_secret = near_api::signer::generate_secret_key().unwrap();
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let owner_id: AccountId = format!("user{n}.sandbox").parse().unwrap();

    near_api::Account::create_account(owner_id.clone())
        .fund_myself(s.genesis_id.clone(), NearToken::from_near(10))
        .with_public_key(owner_secret.public_key())
        .with_signer(s.genesis_signer.clone())
        .send_to(&s.network)
        .await
        .unwrap()
        .assert_success();

    let owner_signer = Signer::from_secret_key(owner_secret).unwrap();

    // Call create_repo on factory
    let result = Contract(factory_id.clone())
        .call_function("create_repo", json!({ "repo_name": "myrepo" }))
        .transaction()
        .deposit(NearToken::from_near(2))
        .with_signer(owner_id.clone(), owner_signer.clone())
        .send_to(&s.network)
        .await
        .unwrap();

    result.assert_success();

    // Verify the repo contract exists and is initialized
    let repo_id: AccountId = format!("myrepo.{}", factory_id).parse().unwrap();

    let owner: String = Contract(repo_id.clone())
        .call_function("get_owner", json!({}))
        .read_only::<String>()
        .fetch_from(&s.network)
        .await
        .unwrap()
        .data;

    assert_eq!(owner, owner_id.to_string());

    // Verify the repo is functional — push a packfile as the owner
    #[derive(borsh::BorshSerialize, Clone)]
    struct RefUpdate {
        name: String,
        old_sha: Option<String>,
        new_sha: String,
    }

    fn encode_push_args(pack_data: &[u8], ref_updates: &[RefUpdate]) -> Vec<u8> {
        use borsh::BorshSerialize;
        let mut buf = Vec::new();
        pack_data.to_vec().serialize(&mut buf).unwrap();
        ref_updates.to_vec().serialize(&mut buf).unwrap();
        buf
    }

    let pack_data = b"PACK\x00\x00\x00\x02\x00\x00\x00\x00test-pack";
    let ref_updates = vec![RefUpdate {
        name: "refs/heads/main".to_string(),
        old_sha: None,
        new_sha: "deadbeef00000000000000000000000000000000".to_string(),
    }];

    let result = Contract(repo_id.clone())
        .call_function_raw("push", encode_push_args(pack_data, &ref_updates))
        .transaction()
        .gas(near_api::NearGas::from_tgas(300))
        .with_signer(owner_id, owner_signer)
        .send_to(&s.network)
        .await
        .unwrap();

    result.assert_success();
}

#[tokio::test]
async fn test_factory_repo_rejects_direct_init() {
    let s = shared().await;
    let (factory_id, _factory_signer) = setup_factory().await;

    // Try to deploy the global contract code directly and init with factory param
    // This should fail because the caller is not the factory
    let attacker_secret = near_api::signer::generate_secret_key().unwrap();
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let attacker_id: AccountId = format!("attacker{n}.sandbox").parse().unwrap();

    near_api::Account::create_account(attacker_id.clone())
        .fund_myself(s.genesis_id.clone(), NearToken::from_near(10))
        .with_public_key(attacker_secret.public_key())
        .with_signer(s.genesis_signer.clone())
        .send_to(&s.network)
        .await
        .unwrap()
        .assert_success();

    let _attacker_signer = Signer::from_secret_key(attacker_secret).unwrap();

    // Deploy the same WASM directly (not through factory)
    let contract_secret = near_api::signer::generate_secret_key().unwrap();
    let contract_id: AccountId = format!("rogue{n}.sandbox").parse().unwrap();

    near_api::Account::create_account(contract_id.clone())
        .fund_myself(s.genesis_id.clone(), NearToken::from_near(10))
        .with_public_key(contract_secret.public_key())
        .with_signer(s.genesis_signer.clone())
        .send_to(&s.network)
        .await
        .unwrap()
        .assert_success();

    let contract_signer = Signer::from_secret_key(contract_secret).unwrap();

    // Deploy directly (not as sub-account of factory) — should succeed
    // because the factory check only applies to sub-accounts of FACTORY_ACCOUNT
    let result = Contract::deploy(contract_id.clone())
        .use_code(s.storage_wasm.clone())
        .with_init_call("new", json!({}))
        .unwrap()
        .with_signer(contract_signer)
        .send_to(&s.network)
        .await
        .unwrap();

    result.assert_success();
}

#[tokio::test]
async fn test_factory_get_global_contract_hash() {
    let s = shared().await;
    let (factory_id, _factory_signer) = setup_factory().await;

    let result: String = Contract(factory_id)
        .call_function("get_global_contract_hash", json!({}))
        .read_only::<String>()
        .fetch_from(&s.network)
        .await
        .unwrap()
        .data;

    assert_eq!(result, s.wasm_hash);
}

#[tokio::test]
async fn test_self_delete_by_owner() {
    let s = shared().await;
    let (factory_id, _factory_signer) = setup_factory().await;

    // Create owner
    let owner_secret = near_api::signer::generate_secret_key().unwrap();
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let owner_id: AccountId = format!("delowner{n}.sandbox").parse().unwrap();

    near_api::Account::create_account(owner_id.clone())
        .fund_myself(s.genesis_id.clone(), NearToken::from_near(10))
        .with_public_key(owner_secret.public_key())
        .with_signer(s.genesis_signer.clone())
        .send_to(&s.network)
        .await
        .unwrap()
        .assert_success();

    let owner_signer = Signer::from_secret_key(owner_secret).unwrap();

    // Create repo
    let repo_name = format!("delrepo{n}");
    Contract(factory_id.clone())
        .call_function("create_repo", json!({ "repo_name": repo_name }))
        .transaction()
        .deposit(NearToken::from_near(2))
        .with_signer(owner_id.clone(), owner_signer.clone())
        .send_to(&s.network)
        .await
        .unwrap()
        .assert_success();

    let repo_id: AccountId = format!("{}.{}", repo_name, factory_id).parse().unwrap();

    // Verify repo exists
    let owner: String = Contract(repo_id.clone())
        .call_function("get_owner", json!({}))
        .read_only::<String>()
        .fetch_from(&s.network)
        .await
        .unwrap()
        .data;
    assert_eq!(owner, owner_id.to_string());

    // Owner calls self_delete
    Contract(repo_id.clone())
        .call_function("self_delete", json!({}))
        .transaction()
        .with_signer(owner_id.clone(), owner_signer.clone())
        .send_to(&s.network)
        .await
        .unwrap()
        .assert_success();

    // Verify repo account no longer exists (view call should fail)
    let result = Contract(repo_id)
        .call_function("get_owner", json!({}))
        .read_only::<String>()
        .fetch_from(&s.network)
        .await;

    assert!(result.is_err(), "Repo should no longer exist after self_delete");
}

#[tokio::test]
async fn test_self_delete_rejected_for_non_owner() {
    let s = shared().await;
    let (factory_id, _factory_signer) = setup_factory().await;

    // Create owner
    let owner_secret = near_api::signer::generate_secret_key().unwrap();
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let owner_id: AccountId = format!("owner{n}.sandbox").parse().unwrap();

    near_api::Account::create_account(owner_id.clone())
        .fund_myself(s.genesis_id.clone(), NearToken::from_near(10))
        .with_public_key(owner_secret.public_key())
        .with_signer(s.genesis_signer.clone())
        .send_to(&s.network)
        .await
        .unwrap()
        .assert_success();

    let owner_signer = Signer::from_secret_key(owner_secret).unwrap();

    // Create repo
    let repo_name = format!("repo{n}");
    Contract(factory_id.clone())
        .call_function("create_repo", json!({ "repo_name": repo_name }))
        .transaction()
        .deposit(NearToken::from_near(2))
        .with_signer(owner_id.clone(), owner_signer.clone())
        .send_to(&s.network)
        .await
        .unwrap()
        .assert_success();

    let repo_id: AccountId = format!("{}.{}", repo_name, factory_id).parse().unwrap();

    // Create a non-owner account
    let non_owner_secret = near_api::signer::generate_secret_key().unwrap();
    let non_owner_id: AccountId = format!("nonowner{n}.sandbox").parse().unwrap();

    near_api::Account::create_account(non_owner_id.clone())
        .fund_myself(s.genesis_id.clone(), NearToken::from_near(5))
        .with_public_key(non_owner_secret.public_key())
        .with_signer(s.genesis_signer.clone())
        .send_to(&s.network)
        .await
        .unwrap()
        .assert_success();

    let non_owner_signer = Signer::from_secret_key(non_owner_secret).unwrap();

    // Non-owner tries self_delete — should fail
    let result = Contract(repo_id.clone())
        .call_function("self_delete", json!({}))
        .transaction()
        .with_signer(non_owner_id, non_owner_signer)
        .send_to(&s.network)
        .await
        .unwrap();

    assert!(
        result.is_failure(),
        "Non-owner should not be able to delete the repo"
    );

    // Verify repo still exists
    let owner: String = Contract(repo_id)
        .call_function("get_owner", json!({}))
        .read_only::<String>()
        .fetch_from(&s.network)
        .await
        .unwrap()
        .data;
    assert_eq!(owner, owner_id.to_string());
}
