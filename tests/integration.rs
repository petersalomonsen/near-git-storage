use std::sync::{Arc, atomic::{AtomicU32, Ordering}};

use borsh::{BorshDeserialize, BorshSerialize};
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

/// Test-side mirror of the contract's GitObject (borsh-serialized).
#[derive(BorshSerialize, BorshDeserialize, Clone)]
struct GitObject {
    sha: String,
    obj_type: String,
    data: Vec<u8>,
}

/// Test-side mirror of RetrievedObject (borsh-deserialized).
#[derive(BorshSerialize, BorshDeserialize)]
struct RetrievedObject {
    obj_type: String,
    data: Vec<u8>,
}

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

/// Helper: compute git SHA-1 for a raw object (same algorithm as git).
fn git_sha(obj_type: &str, data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let header = format!("{} {}\0", obj_type, data.len());
    let mut hasher = Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(data);
    let result = hasher.finalize();
    result.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Build a GitObject (SHA computed client-side).
fn blob(data: &[u8]) -> GitObject {
    GitObject {
        sha: git_sha("blob", data),
        obj_type: "blob".to_string(),
        data: data.to_vec(),
    }
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
    /// Call a JSON contract method as the owner.
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

    /// Push git objects via borsh.
    async fn push_objects(&self, objects: Vec<GitObject>) {
        Contract(self.contract_id.clone())
            .call_function_borsh("push_objects", &objects)
            .transaction()
            .with_signer(self.owner_id.clone(), self.owner_signer.clone())
            .send_to(&self.network)
            .await
            .unwrap()
            .assert_success();
    }

    /// Push git objects via borsh, return raw TransactionResult.
    async fn push_objects_raw(
        &self,
        objects: Vec<GitObject>,
    ) -> near_api::types::transaction::result::TransactionResult {
        Contract(self.contract_id.clone())
            .call_function_borsh("push_objects", &objects)
            .transaction()
            .with_signer(self.owner_id.clone(), self.owner_signer.clone())
            .send_to(&self.network)
            .await
            .unwrap()
    }

    /// Retrieve objects by SHA via borsh view call.
    async fn get_objects(&self, shas: Vec<String>) -> Vec<(String, Option<RetrievedObject>)> {
        Contract(self.contract_id.clone())
            .call_function_borsh("get_objects", &shas)
            .read_only_borsh::<Vec<(String, Option<RetrievedObject>)>>()
            .fetch_from(&self.network)
            .await
            .unwrap()
            .data
    }

    /// Call a JSON view method with no args and deserialize the result.
    async fn view_no_args<T: serde::de::DeserializeOwned + Send + Sync>(&self, method: &str) -> T {
        Contract(self.contract_id.clone())
            .call_function(method, json!({}))
            .read_only::<T>()
            .fetch_from(&self.network)
            .await
            .unwrap()
            .data
    }

    /// Call a JSON view method with args and deserialize the result.
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
}

#[tokio::test]
async fn test_push_and_retrieve_object() {
    let ctx = setup().await;

    let data = b"hello world";
    let obj = blob(data);
    let sha = obj.sha.clone();

    ctx.push_objects(vec![obj]).await;

    let objects = ctx.get_objects(vec![sha.clone()]).await;
    assert_eq!(objects.len(), 1);
    let retrieved = objects[0].1.as_ref().unwrap();
    assert_eq!(retrieved.obj_type, "blob");
    assert_eq!(retrieved.data, data);
}

#[tokio::test]
async fn test_push_multiple_objects() {
    let ctx = setup().await;

    let obj1 = blob(b"file one");
    let obj2 = blob(b"file two");
    let sha1 = obj1.sha.clone();
    let sha2 = obj2.sha.clone();

    ctx.push_objects(vec![obj1, obj2]).await;

    let objects = ctx.get_objects(vec![sha1.clone(), sha2.clone()]).await;
    assert_eq!(objects.len(), 2);
    assert_eq!(objects[0].1.as_ref().unwrap().data, b"file one");
    assert_eq!(objects[1].1.as_ref().unwrap().data, b"file two");
}

#[tokio::test]
async fn test_register_push_stores_mappings_and_updates_refs() {
    let ctx = setup().await;

    let obj = blob(b"hello world");
    let sha = obj.sha.clone();

    let push_result = ctx.push_objects_raw(vec![obj]).await;
    let full = push_result.into_full().unwrap();
    let tx_hash = full.outcome().transaction_hash.to_string();
    full.assert_success();

    ctx.owner_call(
        "register_push",
        json!({
            "tx_hash": tx_hash,
            "object_shas": [&sha],
            "ref_updates": [{
                "name": "refs/heads/main",
                "old_sha": null,
                "new_sha": &sha
            }]
        }),
    )
    .await
    .assert_success();

    let refs: Vec<(String, String)> = ctx.view_no_args("get_refs").await;
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].0, "refs/heads/main");
    assert_eq!(refs[0].1, sha);

    let locations: Vec<(String, Option<String>)> = ctx
        .view("get_object_locations", json!({ "shas": [&sha] }))
        .await;
    assert_eq!(locations[0].1, Some(tx_hash));
}

#[tokio::test]
async fn test_ref_update_cas_rejects_stale_old_sha() {
    let ctx = setup().await;

    let obj1 = blob(b"version 1");
    let sha1 = obj1.sha.clone();

    let push_result = ctx.push_objects_raw(vec![obj1]).await;
    let tx_hash1 = push_result.into_full().unwrap().outcome().transaction_hash.to_string();

    ctx.owner_call(
        "register_push",
        json!({
            "tx_hash": tx_hash1,
            "object_shas": [&sha1],
            "ref_updates": [{ "name": "refs/heads/main", "old_sha": null, "new_sha": &sha1 }]
        }),
    )
    .await
    .assert_success();

    let obj2 = blob(b"version 2");
    let sha2 = obj2.sha.clone();
    let push_result2 = ctx.push_objects_raw(vec![obj2]).await;
    let tx_hash2 = push_result2.into_full().unwrap().outcome().transaction_hash.to_string();

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

    assert!(result.is_failure(), "Expected CAS failure");

    let refs: Vec<(String, String)> = ctx.view_no_args("get_refs").await;
    assert_eq!(refs[0].1, sha1);
}

#[tokio::test]
async fn test_non_owner_cannot_push() {
    let ctx = setup().await;
    let shared = get_shared_sandbox().await;

    let non_owner_secret = near_api::signer::generate_secret_key().unwrap();
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let non_owner_id: AccountId = format!("nonowner{n}.sandbox").parse().unwrap();

    near_api::Account::create_account(non_owner_id.clone())
        .fund_myself(shared.genesis_id.clone(), NearToken::from_near(10))
        .with_public_key(non_owner_secret.public_key())
        .with_signer(shared.genesis_signer.clone())
        .send_to(&ctx.network)
        .await
        .unwrap()
        .assert_success();

    let non_owner_signer = Signer::from_secret_key(non_owner_secret).unwrap();

    let objects = vec![blob(b"hacker data")];
    let result = Contract(ctx.contract_id.clone())
        .call_function_borsh("push_objects", &objects)
        .transaction()
        .with_signer(non_owner_id, non_owner_signer)
        .send_to(&ctx.network)
        .await
        .unwrap();

    assert!(result.is_failure(), "Non-owner should not be able to push");
}

#[tokio::test]
async fn test_get_refs_empty() {
    let ctx = setup().await;
    let refs: Vec<(String, String)> = ctx.view_no_args("get_refs").await;
    assert!(refs.is_empty());
}

#[tokio::test]
async fn test_get_objects_missing() {
    let ctx = setup().await;
    let objects = ctx.get_objects(vec!["0000000000000000000000000000000000000000".to_string()]).await;
    assert_eq!(objects.len(), 1);
    assert!(objects[0].1.is_none());
}

#[tokio::test]
async fn test_duplicate_push_is_idempotent() {
    let ctx = setup().await;

    let obj = blob(b"same data");
    let sha = obj.sha.clone();

    ctx.push_objects(vec![obj.clone()]).await;
    ctx.push_objects(vec![obj]).await;

    let objects = ctx.get_objects(vec![sha]).await;
    assert_eq!(objects[0].1.as_ref().unwrap().data, b"same data");
}

#[tokio::test]
async fn test_stores_compressed_data_verbatim() {
    let ctx = setup().await;

    // Simulate client sending "compressed" bytes — contract stores as-is
    let compressed = vec![0x78, 0x9c, 0xab, 0xca]; // fake zlib header
    let obj = GitObject {
        sha: "deadbeef00000000000000000000000000000000".to_string(),
        obj_type: "blob".to_string(),
        data: compressed.clone(),
    };

    ctx.push_objects(vec![obj]).await;

    let objects = ctx.get_objects(vec!["deadbeef00000000000000000000000000000000".to_string()]).await;
    assert_eq!(objects[0].1.as_ref().unwrap().data, compressed);
}
