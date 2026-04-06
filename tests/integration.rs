use std::io::Write as _;
use std::sync::{Arc, atomic::{AtomicU32, Ordering}};

use borsh::{BorshDeserialize, BorshSerialize};
use near_api::{AccountId, Contract, NearToken, Signer};
use near_sandbox::Sandbox;
use serde_json::json;
use tokio::sync::OnceCell;

const WASM_FILEPATH: &str = "res/near_git_storage.wasm";
const FACTORY_WASM: &str = "res/near_git_factory.wasm";

static TEST_COUNTER: AtomicU32 = AtomicU32::new(0);
static SHARED_SANDBOX: OnceCell<SharedSandbox> = OnceCell::const_new();

/// Borsh-serialized ref update (matches contract's RefUpdate).
#[derive(BorshSerialize, BorshDeserialize, Clone)]
struct RefUpdate {
    name: String,
    old_sha: Option<String>,
    new_sha: String,
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
    #[allow(dead_code)]
    global_id: AccountId,
    wasm_hash: String,
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
                .expect("Contract WASM not found. Run `./build.sh` first.");
            let factory_wasm = std::fs::read(FACTORY_WASM)
                .expect("Factory WASM not found. Run `./build.sh` first.");

            use sha2::{Digest, Sha256};
            let wasm_hash: String = Sha256::digest(&wasm)
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect();

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

            Contract::deploy_global_contract_code(wasm.clone())
                .as_hash()
                .with_signer(global_id.clone(), Signer::from_secret_key(global_secret).unwrap())
                .send_to(&network)
                .await
                .unwrap()
                .assert_success();

            SharedSandbox {
                sandbox, network, genesis_id, genesis_signer,
                wasm, factory_wasm, global_id, wasm_hash,
            }
        })
        .await
}

struct TestContext {
    network: near_api::NetworkConfig,
    owner_id: AccountId,
    owner_signer: Arc<Signer>,
    contract_id: AccountId,
}

async fn setup() -> TestContext {
    let shared = get_shared_sandbox().await;
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);

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
        .with_init_call("new", json!({ "global_contract_hash": &shared.wasm_hash }))
        .unwrap()
        .with_signer(Signer::from_secret_key(factory_secret).unwrap())
        .send_to(&shared.network)
        .await
        .unwrap()
        .assert_success();

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

    TestContext { network: shared.network.clone(), owner_id, owner_signer, contract_id }
}

impl TestContext {
    async fn push_pack(&self, pack_data: &[u8], ref_updates: &[RefUpdate]) {
        Contract(self.contract_id.clone())
            .call_function_borsh("push", (pack_data, ref_updates))
            .transaction()
            .gas(near_api::NearGas::from_tgas(300))
            .with_signer(self.owner_id.clone(), self.owner_signer.clone())
            .send_to(&self.network)
            .await
            .unwrap()
            .assert_success();
    }

    async fn get_packs(&self, from_index: u32) -> Vec<Vec<u8>> {
        Contract(self.contract_id.clone())
            .call_function_borsh("get_packs", &from_index)
            .read_only_borsh::<Vec<Vec<u8>>>()
            .fetch_from(&self.network)
            .await
            .unwrap()
            .data
    }

    async fn get_pack_count(&self) -> u32 {
        Contract(self.contract_id.clone())
            .call_function("get_pack_count", json!({}))
            .read_only::<u32>()
            .fetch_from(&self.network)
            .await
            .unwrap()
            .data
    }

    async fn get_storage_bytes(&self) -> u64 {
        near_api::Account(self.contract_id.clone())
            .view()
            .fetch_from(&self.network)
            .await
            .unwrap()
            .data
            .storage_usage
    }

    async fn get_refs(&self) -> Vec<(String, String)> {
        Contract(self.contract_id.clone())
            .call_function("get_refs", json!({}))
            .read_only::<Vec<(String, String)>>()
            .fetch_from(&self.network)
            .await
            .unwrap()
            .data
    }
}

#[tokio::test]
async fn test_push_pack_and_retrieve() {
    let ctx = setup().await;

    let pack_data = b"PACK\x00\x00\x00\x02\x00\x00\x00\x00fake-pack-data-here";
    let ref_updates = vec![RefUpdate {
        name: "refs/heads/main".to_string(),
        old_sha: None,
        new_sha: "aaaa000000000000000000000000000000000001".to_string(),
    }];

    ctx.push_pack(pack_data, &ref_updates).await;

    // Verify refs
    let refs = ctx.get_refs().await;
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].0, "refs/heads/main");
    assert_eq!(refs[0].1, "aaaa000000000000000000000000000000000001");

    // Verify pack count
    assert_eq!(ctx.get_pack_count().await, 1);

    // Verify pack data
    let packs = ctx.get_packs(0).await;
    assert_eq!(packs.len(), 1);
    assert_eq!(packs[0], pack_data);
}

#[tokio::test]
async fn test_incremental_packs() {
    let ctx = setup().await;

    // Push pack 1
    ctx.push_pack(b"pack-one", &[RefUpdate {
        name: "refs/heads/main".to_string(),
        old_sha: None,
        new_sha: "aaaa000000000000000000000000000000000001".to_string(),
    }]).await;

    // Push pack 2
    ctx.push_pack(b"pack-two", &[RefUpdate {
        name: "refs/heads/main".to_string(),
        old_sha: Some("aaaa000000000000000000000000000000000001".to_string()),
        new_sha: "aaaa000000000000000000000000000000000002".to_string(),
    }]).await;

    assert_eq!(ctx.get_pack_count().await, 2);

    // Get all packs
    let all = ctx.get_packs(0).await;
    assert_eq!(all.len(), 2);
    assert_eq!(all[0], b"pack-one");
    assert_eq!(all[1], b"pack-two");

    // Get only new packs (incremental)
    let new_only = ctx.get_packs(1).await;
    assert_eq!(new_only.len(), 1);
    assert_eq!(new_only[0], b"pack-two");
}

#[tokio::test]
async fn test_empty_push_updates_refs() {
    let ctx = setup().await;

    // Push with data first
    ctx.push_pack(b"pack-data", &[RefUpdate {
        name: "refs/heads/main".to_string(),
        old_sha: None,
        new_sha: "aaaa000000000000000000000000000000000001".to_string(),
    }]).await;

    // Empty push just updates ref
    ctx.push_pack(b"", &[RefUpdate {
        name: "refs/heads/main".to_string(),
        old_sha: Some("aaaa000000000000000000000000000000000001".to_string()),
        new_sha: "aaaa000000000000000000000000000000000002".to_string(),
    }]).await;

    // Should still have only 1 pack (empty data not stored)
    assert_eq!(ctx.get_pack_count().await, 1);

    // Ref should be updated
    let refs = ctx.get_refs().await;
    assert_eq!(refs[0].1, "aaaa000000000000000000000000000000000002");
}

#[tokio::test]
async fn test_ref_cas_failure() {
    let ctx = setup().await;

    ctx.push_pack(b"pack", &[RefUpdate {
        name: "refs/heads/main".to_string(),
        old_sha: None,
        new_sha: "aaaa000000000000000000000000000000000001".to_string(),
    }]).await;

    // Wrong old_sha should fail
    let result = Contract(ctx.contract_id.clone())
        .call_function_borsh("push", (
            b"pack2".to_vec(),
            vec![RefUpdate {
                name: "refs/heads/main".to_string(),
                old_sha: Some("0000000000000000000000000000000000000000".to_string()),
                new_sha: "aaaa000000000000000000000000000000000002".to_string(),
            }],
        ))
        .transaction()
        .gas(near_api::NearGas::from_tgas(300))
        .with_signer(ctx.owner_id.clone(), ctx.owner_signer.clone())
        .send_to(&ctx.network)
        .await
        .unwrap();

    assert!(result.is_failure(), "Expected CAS failure");
}

#[tokio::test]
async fn test_get_refs_empty() {
    let ctx = setup().await;
    let refs = ctx.get_refs().await;
    assert!(refs.is_empty());
}

#[tokio::test]
async fn test_self_delete_clears_storage() {
    let ctx = setup().await;

    // Push many packs to build up significant storage
    for i in 0..20u32 {
        // ~1KB of data per pack
        let data: Vec<u8> = (0..1000).map(|j| ((i * 1000 + j) % 256) as u8).collect();
        let sha = format!("aaaa{:036}", i);
        let old_sha = if i == 0 {
            None
        } else {
            Some(format!("aaaa{:036}", i - 1))
        };
        ctx.push_pack(&data, &[RefUpdate {
            name: "refs/heads/main".to_string(),
            old_sha,
            new_sha: sha,
        }]).await;
    }

    assert_eq!(ctx.get_pack_count().await, 20);

    let storage_before = ctx.get_storage_bytes().await;
    eprintln!("Storage before clear: {} bytes ({} packs)", storage_before, 20);
    assert!(storage_before > 20_000, "Should have significant storage from 20 packs");

    // Step 1: clear storage
    Contract(ctx.contract_id.clone())
        .call_function("clear_storage", json!({}))
        .transaction()
        .gas(near_api::NearGas::from_tgas(300))
        .with_signer(ctx.owner_id.clone(), ctx.owner_signer.clone())
        .send_to(&ctx.network)
        .await
        .unwrap()
        .assert_success();

    let storage_after_clear = ctx.get_storage_bytes().await;
    eprintln!("Storage after clear: {} bytes", storage_after_clear);
    assert!(storage_after_clear < storage_before / 2, "Storage should be significantly reduced");

    // Step 2: delete account
    Contract(ctx.contract_id.clone())
        .call_function("self_delete", json!({}))
        .transaction()
        .gas(near_api::NearGas::from_tgas(300))
        .with_signer(ctx.owner_id.clone(), ctx.owner_signer.clone())
        .send_to(&ctx.network)
        .await
        .unwrap()
        .assert_success();

    // Account should no longer exist
    let view_result = near_api::Account(ctx.contract_id.clone())
        .view()
        .fetch_from(&ctx.network)
        .await;
    assert!(view_result.is_err(), "Account should be deleted");
}

/// Helper: create a git repo in a temp dir with initial content, return the path.
fn create_test_repo(name: &str, content: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("near-git-test-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap()
    };

    run(&["init"]);
    run(&["-c", "user.name=test", "-c", "user.email=test@test", "commit", "--allow-empty", "-m", "init"]);
    std::fs::write(dir.join("file.txt"), content).unwrap();
    run(&["add", "."]);
    run(&["-c", "user.name=test", "-c", "user.email=test@test", "commit", "-m", "add file"]);

    dir
}

/// Helper: get HEAD SHA of a git repo.
fn get_head_sha(dir: &std::path::Path) -> String {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Helper: build a packfile from a git repo using `git pack-objects`.
fn build_pack(dir: &std::path::Path, new_sha: &str, old_sha: Option<&str>) -> Vec<u8> {
    let mut child = std::process::Command::new("git")
        .args(["pack-objects", "--stdout", "--delta-base-offset", "--revs", "--thin"])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", new_sha).unwrap();
        if let Some(old) = old_sha {
            writeln!(stdin, "--not").unwrap();
            writeln!(stdin, "{}", old).unwrap();
        }
    }

    child.wait_with_output().unwrap().stdout
}

#[tokio::test]
async fn test_packfile_storage_initial_push() {
    let ctx = setup().await;

    // Create a repo with a ~8KB file
    let content: String = (0..200)
        .map(|i| format!("line {}: hello world content here padding\n", i))
        .collect();
    let dir = create_test_repo("initial", &content);
    let head = get_head_sha(&dir);
    let pack = build_pack(&dir, &head, None);

    let initial_storage = ctx.get_storage_bytes().await;

    ctx.push_pack(&pack, &[RefUpdate {
        name: "refs/heads/main".to_string(),
        old_sha: None,
        new_sha: head,
    }]).await;

    let after_push = ctx.get_storage_bytes().await;
    let storage_used = after_push - initial_storage;

    eprintln!("=== Initial push ===");
    eprintln!("Content: {} bytes, Packfile: {} bytes, Storage: {} bytes",
        content.len(), pack.len(), storage_used);

    // Packfile should be significantly smaller than raw content due to zlib
    assert!(pack.len() < content.len(), "Pack should be smaller than raw content");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_packfile_storage_incremental_push() {
    let ctx = setup().await;

    // Create a repo with a ~8KB file
    let content_v1: String = (0..200)
        .map(|i| format!("line {}: hello world content here padding\n", i))
        .collect();
    let dir = create_test_repo("incremental", &content_v1);
    let sha_v1 = get_head_sha(&dir);
    let pack_v1 = build_pack(&dir, &sha_v1, None);

    ctx.push_pack(&pack_v1, &[RefUpdate {
        name: "refs/heads/main".to_string(),
        old_sha: None,
        new_sha: sha_v1.clone(),
    }]).await;

    let after_v1 = ctx.get_storage_bytes().await;

    // Make a small change (add one line)
    let mut content_v2 = content_v1.clone();
    content_v2.insert_str(0, "// Added a single comment line\n");
    std::fs::write(dir.join("file.txt"), &content_v2).unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["-c", "user.name=test", "-c", "user.email=test@test", "commit", "-m", "small edit"])
        .current_dir(&dir)
        .output()
        .unwrap();

    let sha_v2 = get_head_sha(&dir);
    let pack_v2 = build_pack(&dir, &sha_v2, Some(&sha_v1));

    ctx.push_pack(&pack_v2, &[RefUpdate {
        name: "refs/heads/main".to_string(),
        old_sha: Some(sha_v1),
        new_sha: sha_v2,
    }]).await;

    let after_v2 = ctx.get_storage_bytes().await;

    let v2_storage = after_v2 - after_v1;

    eprintln!("=== Incremental push (thin pack) ===");
    eprintln!("Content change: 1 line added to {} byte file", content_v1.len());
    eprintln!("Full pack v1: {} bytes", pack_v1.len());
    eprintln!("Thin pack v2: {} bytes", pack_v2.len());
    eprintln!("Storage increase for v2: {} bytes", v2_storage);

    // Thin pack for a small change should be very small (< 1KB)
    assert!(
        pack_v2.len() < 1000,
        "Thin pack for 1-line change should be < 1KB, got {} bytes", pack_v2.len()
    );

    // Storage increase should be proportional to the thin pack, not the full file
    assert!(
        v2_storage < 2000,
        "Storage increase for 1-line change should be < 2KB, got {} bytes", v2_storage
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_ofs_delta_packfile_roundtrip() {
    // Packfiles from `git pack-objects --delta-base-offset` contain OFS_DELTA
    // entries. The git-server and service worker parse these packs to merge
    // objects for clone. This test verifies the full roundtrip: create pack
    // with OFS_DELTA, store on-chain, retrieve, parse.
    //
    // Without OFS_DELTA support in packfile::parse(), this test fails with
    // "ofs_delta objects not yet supported".
    let ctx = setup().await;

    let dir = std::env::temp_dir().join(format!("near-git-test-ofsdelta-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap()
    };

    run(&["init"]);

    // Create a ~4KB file so delta compression kicks in
    let content_v1: String = (0..100)
        .map(|i| format!("line {}: some content that will be mostly unchanged\n", i))
        .collect();
    std::fs::write(dir.join("file.txt"), &content_v1).unwrap();
    run(&["add", "."]);
    run(&["-c", "user.name=test", "-c", "user.email=test@test", "commit", "-m", "v1"]);

    // Small edits to force OFS_DELTA in the packfile
    let mut content_v2 = content_v1.clone();
    content_v2.insert_str(0, "// small edit\n");
    std::fs::write(dir.join("file.txt"), &content_v2).unwrap();
    run(&["add", "."]);
    run(&["-c", "user.name=test", "-c", "user.email=test@test", "commit", "-m", "v2"]);

    let mut content_v3 = content_v2.clone();
    content_v3.push_str("// appended line\n");
    std::fs::write(dir.join("file.txt"), &content_v3).unwrap();
    run(&["add", "."]);
    run(&["-c", "user.name=test", "-c", "user.email=test@test", "commit", "-m", "v3"]);

    let head = get_head_sha(&dir);
    let pack = build_pack(&dir, &head, None);

    eprintln!("=== OFS_DELTA roundtrip test ===");
    eprintln!("Pack size: {} bytes", pack.len());

    // Parse — OFS_DELTA should be resolved, no unresolved deltas
    let parsed = git_core::packfile::parse(&pack).unwrap();
    eprintln!("Parsed: {} objects, {} unresolved deltas", parsed.objects.len(), parsed.deltas.len());

    assert!(parsed.deltas.is_empty(), "OFS_DELTA should be resolved inline");
    assert!(parsed.objects.len() >= 6, "Expected >=6 objects, got {}", parsed.objects.len());

    // Verify latest file content is in the resolved objects
    assert!(
        parsed.objects.iter().any(|o| o.obj_type == "blob" && o.data == content_v3.as_bytes()),
        "Should find v3 blob content in resolved objects"
    );

    // Store on-chain, retrieve, re-parse (simulates git-server/service-worker clone)
    ctx.push_pack(&pack, &[RefUpdate {
        name: "refs/heads/main".to_string(),
        old_sha: None,
        new_sha: head,
    }]).await;

    let packs = ctx.get_packs(0).await;
    let reparsed = git_core::packfile::parse(&packs[0]).unwrap();
    assert_eq!(reparsed.objects.len(), parsed.objects.len());
    assert!(reparsed.deltas.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}
