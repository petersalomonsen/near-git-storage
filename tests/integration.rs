use std::sync::{Arc, atomic::{AtomicU32, Ordering}};

use near_api::{AccountId, Contract, NearToken, Signer};
use near_sandbox::Sandbox;
use serde_json::json;
use tokio::sync::OnceCell;

const WASM_FILEPATH: &str = "res/near_git_storage.wasm";
const FACTORY_WASM: &str = "res/near_git_factory.wasm";

/// Counter for generating unique account names per test.
static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Shared sandbox instance across all tests.
static SHARED_SANDBOX: OnceCell<SharedSandbox> = OnceCell::const_new();

struct SharedSandbox {
    #[allow(dead_code)]
    sandbox: Sandbox,
    network: near_api::NetworkConfig,
    genesis_id: AccountId,
    genesis_signer: Arc<Signer>,
    #[allow(dead_code)]
    wasm: Vec<u8>,
    factory_wasm: Vec<u8>,
    global_id: AccountId,
}

async fn get_shared_sandbox() -> &'static SharedSandbox {
    SHARED_SANDBOX
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

            let wasm = std::fs::read(WASM_FILEPATH)
                .expect("Contract WASM not found. Run `./build.sh` first to build the contract.");
            let factory_wasm = std::fs::read(FACTORY_WASM)
                .expect("Factory WASM not found. Run `./build.sh` first.");

            // Deploy git-storage WASM as a global contract tied to a "global" account
            let global_secret = near_api::signer::generate_secret_key().unwrap();
            let global_id: AccountId = "global.sandbox".parse().unwrap();

            near_api::Account::create_account(global_id.clone())
                .fund_myself(genesis_id.clone(), NearToken::from_near(50))
                .with_public_key(global_secret.public_key())
                .with_signer(genesis_signer.clone())
                .send_to(&network)
                .await
                .unwrap()
                .assert_success();

            let global_signer = Signer::from_secret_key(global_secret).unwrap();

            Contract::deploy_global_contract_code(wasm.clone())
                .as_account_id(global_id.clone())
                .with_signer(global_signer)
                .send_to(&network)
                .await
                .unwrap()
                .assert_success();

            SharedSandbox {
                sandbox,
                network,
                genesis_id,
                genesis_signer,
                wasm,
                factory_wasm,
                global_id,
            }
        })
        .await
}

/// Helper: compute git SHA-1 for a raw object (same algorithm as the contract).
fn git_sha(obj_type: &str, data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let header = format!("{} {}\0", obj_type, data.len());
    let mut hasher = Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(data);
    let result = hasher.finalize();
    result.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Helper: base64 encode bytes.
fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

struct TestContext {
    network: near_api::NetworkConfig,
    owner_id: AccountId,
    owner_signer: Arc<Signer>,
    contract_id: AccountId,
}

/// Set up a fresh contract + owner within the shared sandbox.
/// Deploys the factory, then creates a repo via the factory.
async fn setup() -> TestContext {
    let shared = get_shared_sandbox().await;
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);

    // Create owner account
    let owner_secret = near_api::signer::generate_secret_key().unwrap();
    let owner_id: AccountId = format!("owner{n}.sandbox").parse().unwrap();

    near_api::Account::create_account(owner_id.clone())
        .fund_myself(shared.genesis_id.clone(), NearToken::from_near(50))
        .with_public_key(owner_secret.public_key())
        .with_signer(shared.genesis_signer.clone())
        .send_to(&shared.network)
        .await
        .unwrap()
        .assert_success();

    let owner_signer = Signer::from_secret_key(owner_secret).unwrap();

    // Deploy factory
    let factory_secret = near_api::signer::generate_secret_key().unwrap();
    let factory_id: AccountId = format!("factory{n}.sandbox").parse().unwrap();

    near_api::Account::create_account(factory_id.clone())
        .fund_myself(shared.genesis_id.clone(), NearToken::from_near(10))
        .with_public_key(factory_secret.public_key())
        .with_signer(shared.genesis_signer.clone())
        .send_to(&shared.network)
        .await
        .unwrap()
        .assert_success();

    Contract::deploy(factory_id.clone())
        .use_code(shared.factory_wasm.clone())
        .with_init_call("new", json!({ "global_contract": shared.global_id.to_string() }))
        .unwrap()
        .with_signer(Signer::from_secret_key(factory_secret).unwrap())
        .send_to(&shared.network)
        .await
        .unwrap()
        .assert_success();

    // Create repo via factory
    let repo_name = format!("repo{n}");
    Contract(factory_id.clone())
        .call_function("create_repo", json!({ "repo_name": repo_name }))
        .transaction()
        .deposit(NearToken::from_near(2))
        .with_signer(owner_id.clone(), owner_signer.clone())
        .send_to(&shared.network)
        .await
        .unwrap()
        .assert_success();

    let contract_id: AccountId = format!("{}.{}", repo_name, factory_id).parse().unwrap();

    TestContext {
        network: shared.network.clone(),
        owner_id,
        owner_signer,
        contract_id,
    }
}

impl TestContext {
    /// Call a contract method as the owner, returning the TransactionResult.
    async fn owner_call(
        &self,
        method: &str,
        args: serde_json::Value,
    ) -> near_api::types::transaction::result::TransactionResult {
        Contract(self.contract_id.clone())
            .call_function(method, args)
            .transaction()
            .with_signer(self.owner_id.clone(), self.owner_signer.clone())
            .send_to(&self.network)
            .await
            .unwrap()
    }

    /// Call a view method with args and deserialize the result.
    async fn view<T: serde::de::DeserializeOwned + Send + Sync>(
        &self,
        method: &str,
        args: serde_json::Value,
    ) -> T {
        Contract(self.contract_id.clone())
            .call_function(method, args)
            .read_only::<T>()
            .fetch_from(&self.network)
            .await
            .unwrap()
            .data
    }

    /// Call a view method with no args and deserialize the result.
    async fn view_no_args<T: serde::de::DeserializeOwned + Send + Sync>(&self, method: &str) -> T {
        Contract(self.contract_id.clone())
            .call_function(method, json!({}))
            .read_only::<T>()
            .fetch_from(&self.network)
            .await
            .unwrap()
            .data
    }
}

#[tokio::test]
async fn test_push_objects_returns_correct_shas() {
    let ctx = setup().await;

    let blob_data = b"hello world";
    let expected_sha = git_sha("blob", blob_data);

    let result = ctx
        .owner_call(
            "push_objects",
            json!({
                "objects": [
                    {
                        "obj_type": "blob",
                        "data": b64(blob_data)
                    }
                ]
            }),
        )
        .await;

    let success = result.assert_success();
    let push_result: serde_json::Value = success.json().unwrap();
    let shas = push_result["shas"].as_array().unwrap();

    assert_eq!(shas.len(), 1);
    assert_eq!(shas[0].as_str().unwrap(), expected_sha);
}

#[tokio::test]
async fn test_push_multiple_objects() {
    let ctx = setup().await;

    let blob1 = b"file one";
    let blob2 = b"file two";
    let expected_sha1 = git_sha("blob", blob1);
    let expected_sha2 = git_sha("blob", blob2);

    let result = ctx
        .owner_call(
            "push_objects",
            json!({
                "objects": [
                    { "obj_type": "blob", "data": b64(blob1) },
                    { "obj_type": "blob", "data": b64(blob2) }
                ]
            }),
        )
        .await;

    let success = result.assert_success();
    let push_result: serde_json::Value = success.json().unwrap();
    let shas = push_result["shas"].as_array().unwrap();

    assert_eq!(shas.len(), 2);
    assert_eq!(shas[0].as_str().unwrap(), expected_sha1);
    assert_eq!(shas[1].as_str().unwrap(), expected_sha2);
}

#[tokio::test]
async fn test_register_push_stores_mappings_and_updates_refs() {
    let ctx = setup().await;

    let blob_data = b"hello world";
    let sha = git_sha("blob", blob_data);

    // Push the objects
    let push_result = ctx
        .owner_call(
            "push_objects",
            json!({
                "objects": [
                    { "obj_type": "blob", "data": b64(blob_data) }
                ]
            }),
        )
        .await;

    let full = push_result.into_full().unwrap();
    let tx_hash = full.outcome().transaction_hash.to_string();
    full.assert_success();

    // Register the push with ref updates
    ctx.owner_call(
        "register_push",
        json!({
            "tx_hash": tx_hash,
            "object_shas": [&sha],
            "ref_updates": [
                {
                    "name": "refs/heads/main",
                    "old_sha": null,
                    "new_sha": &sha
                }
            ]
        }),
    )
    .await
    .assert_success();

    // Verify refs
    let refs: Vec<(String, String)> = ctx.view_no_args("get_refs").await;

    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].0, "refs/heads/main");
    assert_eq!(refs[0].1, sha);

    // Verify object locations
    let locations: Vec<(String, Option<String>)> = ctx
        .view("get_object_locations", json!({ "shas": [&sha] }))
        .await;

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].0, sha);
    assert_eq!(locations[0].1, Some(tx_hash));
}

#[tokio::test]
async fn test_ref_update_cas_rejects_stale_old_sha() {
    let ctx = setup().await;

    let blob1 = b"version 1";
    let sha1 = git_sha("blob", blob1);

    // Push and register the first version
    let push_result = ctx
        .owner_call(
            "push_objects",
            json!({
                "objects": [{ "obj_type": "blob", "data": b64(blob1) }]
            }),
        )
        .await;

    let tx_hash1 = push_result.into_full().unwrap().outcome().transaction_hash.to_string();
    // push_result consumed by into_full above

    ctx.owner_call(
        "register_push",
        json!({
            "tx_hash": tx_hash1,
            "object_shas": [&sha1],
            "ref_updates": [{
                "name": "refs/heads/main",
                "old_sha": null,
                "new_sha": &sha1
            }]
        }),
    )
    .await
    .assert_success();

    // Push a second version
    let blob2 = b"version 2";
    let sha2 = git_sha("blob", blob2);

    let push_result2 = ctx
        .owner_call(
            "push_objects",
            json!({
                "objects": [{ "obj_type": "blob", "data": b64(blob2) }]
            }),
        )
        .await;

    let tx_hash2 = push_result2.into_full().unwrap().outcome().transaction_hash.to_string();
    // push_result2 consumed by into_full above

    // Use a wrong old_sha -- should fail
    let result = ctx
        .owner_call(
            "register_push",
            json!({
                "tx_hash": tx_hash2,
                "object_shas": [&sha2],
                "ref_updates": [{
                    "name": "refs/heads/main",
                    "old_sha": "0000000000000000000000000000000000000000",
                    "new_sha": &sha2
                }]
            }),
        )
        .await;

    assert!(
        result.is_failure(),
        "Expected CAS failure but transaction succeeded"
    );

    // Verify the ref was NOT updated
    let refs: Vec<(String, String)> = ctx.view_no_args("get_refs").await;

    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].1, sha1, "Ref should still point to the original SHA");
}

#[tokio::test]
async fn test_ref_update_cas_succeeds_with_correct_old_sha() {
    let ctx = setup().await;

    let blob1 = b"version 1";
    let sha1 = git_sha("blob", blob1);

    // Create the initial ref
    let push_result = ctx
        .owner_call(
            "push_objects",
            json!({
                "objects": [{ "obj_type": "blob", "data": b64(blob1) }]
            }),
        )
        .await;
    let tx_hash1 = push_result.into_full().unwrap().outcome().transaction_hash.to_string();
    // push_result consumed by into_full above

    ctx.owner_call(
        "register_push",
        json!({
            "tx_hash": tx_hash1,
            "object_shas": [&sha1],
            "ref_updates": [{
                "name": "refs/heads/main",
                "old_sha": null,
                "new_sha": &sha1
            }]
        }),
    )
    .await
    .assert_success();

    // Update with the CORRECT old_sha
    let blob2 = b"version 2";
    let sha2 = git_sha("blob", blob2);

    let push_result2 = ctx
        .owner_call(
            "push_objects",
            json!({
                "objects": [{ "obj_type": "blob", "data": b64(blob2) }]
            }),
        )
        .await;
    let tx_hash2 = push_result2.into_full().unwrap().outcome().transaction_hash.to_string();
    // push_result2 consumed by into_full above

    ctx.owner_call(
        "register_push",
        json!({
            "tx_hash": tx_hash2,
            "object_shas": [&sha2],
            "ref_updates": [{
                "name": "refs/heads/main",
                "old_sha": &sha1,
                "new_sha": &sha2
            }]
        }),
    )
    .await
    .assert_success();

    // Verify the ref was updated
    let refs: Vec<(String, String)> = ctx.view_no_args("get_refs").await;

    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].1, sha2, "Ref should point to the new SHA");
}

#[tokio::test]
async fn test_non_owner_cannot_push() {
    let ctx = setup().await;
    let shared = get_shared_sandbox().await;

    // Create a non-owner account
    let non_owner_secret = near_api::signer::generate_secret_key().unwrap();
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let non_owner_id: AccountId = format!("nonowner{n}.sandbox").parse().unwrap();

    near_api::Account::create_account(non_owner_id.clone())
        .fund_myself(
            shared.genesis_id.clone(),
            near_api::NearToken::from_near(10),
        )
        .with_public_key(non_owner_secret.public_key())
        .with_signer(shared.genesis_signer.clone())
        .send_to(&ctx.network)
        .await
        .unwrap()
        .assert_success();

    let non_owner_signer = Signer::from_secret_key(non_owner_secret).unwrap();

    // Non-owner tries to push_objects
    let result = Contract(ctx.contract_id.clone())
        .call_function(
            "push_objects",
            json!({
                "objects": [{ "obj_type": "blob", "data": b64(b"hacker data") }]
            }),
        )
        .transaction()
        .with_signer(non_owner_id, non_owner_signer)
        .send_to(&ctx.network)
        .await
        .unwrap();

    assert!(
        result.is_failure(),
        "Non-owner should not be able to push objects"
    );
}

#[tokio::test]
async fn test_non_owner_cannot_register_push() {
    let ctx = setup().await;
    let shared = get_shared_sandbox().await;

    // Create a non-owner account
    let non_owner_secret = near_api::signer::generate_secret_key().unwrap();
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let non_owner_id: AccountId = format!("nonowner{n}.sandbox").parse().unwrap();

    near_api::Account::create_account(non_owner_id.clone())
        .fund_myself(
            shared.genesis_id.clone(),
            near_api::NearToken::from_near(10),
        )
        .with_public_key(non_owner_secret.public_key())
        .with_signer(shared.genesis_signer.clone())
        .send_to(&ctx.network)
        .await
        .unwrap()
        .assert_success();

    let non_owner_signer = Signer::from_secret_key(non_owner_secret).unwrap();

    // Non-owner tries to register_push
    let result = Contract(ctx.contract_id.clone())
        .call_function(
            "register_push",
            json!({
                "tx_hash": "fake_tx_hash",
                "object_shas": ["aaaa"],
                "ref_updates": []
            }),
        )
        .transaction()
        .with_signer(non_owner_id, non_owner_signer)
        .send_to(&ctx.network)
        .await
        .unwrap();

    assert!(
        result.is_failure(),
        "Non-owner should not be able to register push"
    );
}

#[tokio::test]
async fn test_get_refs_empty() {
    let ctx = setup().await;

    let refs: Vec<(String, String)> = ctx.view_no_args("get_refs").await;

    assert!(refs.is_empty(), "New contract should have no refs");
}

#[tokio::test]
async fn test_get_object_locations_missing() {
    let ctx = setup().await;

    let locations: Vec<(String, Option<String>)> = ctx
        .view(
            "get_object_locations",
            json!({ "shas": ["0000000000000000000000000000000000000000"] }),
        )
        .await;

    assert_eq!(locations.len(), 1);
    assert_eq!(locations[0].1, None, "Unknown SHA should return None");
}

#[tokio::test]
async fn test_creating_ref_when_one_already_exists_fails() {
    let ctx = setup().await;

    let blob1 = b"data1";
    let sha1 = git_sha("blob", blob1);

    // Create the ref
    let push_result = ctx
        .owner_call(
            "push_objects",
            json!({
                "objects": [{ "obj_type": "blob", "data": b64(blob1) }]
            }),
        )
        .await;
    let tx_hash = push_result.into_full().unwrap().outcome().transaction_hash.to_string();
    // push_result consumed by into_full above

    ctx.owner_call(
        "register_push",
        json!({
            "tx_hash": &tx_hash,
            "object_shas": [&sha1],
            "ref_updates": [{
                "name": "refs/heads/main",
                "old_sha": null,
                "new_sha": &sha1
            }]
        }),
    )
    .await
    .assert_success();

    // Try to create the same ref again (old_sha: null, but ref exists)
    let blob2 = b"data2";
    let sha2 = git_sha("blob", blob2);

    let push_result2 = ctx
        .owner_call(
            "push_objects",
            json!({
                "objects": [{ "obj_type": "blob", "data": b64(blob2) }]
            }),
        )
        .await;
    let tx_hash2 = push_result2.into_full().unwrap().outcome().transaction_hash.to_string();
    // push_result2 consumed by into_full above

    let result = ctx
        .owner_call(
            "register_push",
            json!({
                "tx_hash": &tx_hash2,
                "object_shas": [&sha2],
                "ref_updates": [{
                    "name": "refs/heads/main",
                    "old_sha": null,
                    "new_sha": &sha2
                }]
            }),
        )
        .await;

    assert!(
        result.is_failure(),
        "Creating a ref that already exists (with old_sha=null) should fail"
    );
}
