use git_core::packfile;
use git_core::pktline;

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use tower_http::cors::{Any, CorsLayer};
use base64::Engine;
use borsh::{BorshDeserialize, BorshSerialize};
use near_api::{AccountId, Contract, Signer};
use near_sandbox::Sandbox;
use serde::Deserialize;
use serde_json::json;
use tracing::{error, info};

/// Borsh-serialized ref update for push calls.
#[derive(BorshSerialize, Clone)]
struct RefUpdate {
    name: String,
    old_sha: Option<String>,
    new_sha: String,
}

/// Shared application state.
struct AppState {
    #[allow(dead_code)]
    sandbox: Sandbox,
    network: near_api::NetworkConfig,
    contract_id: AccountId,
    owner_id: AccountId,
    owner_signer: Arc<Signer>,
    /// Raw key strings for the /near-credentials endpoint
    owner_public_key: String,
    owner_secret_key: String,
}

#[derive(Deserialize)]
struct InfoRefsQuery {
    service: String,
}

const CAPABILITIES: &str = "report-status delete-refs";
const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    info!("Starting NEAR sandbox...");
    let sandbox = Sandbox::start_sandbox().await.unwrap();
    let network =
        near_api::NetworkConfig::from_rpc_url("sandbox", sandbox.rpc_addr.parse().unwrap());

    let genesis = near_sandbox::GenesisAccount::default();
    let genesis_id: AccountId = genesis.account_id.to_string().parse().unwrap();
    let genesis_signer = Signer::from_secret_key(genesis.private_key.parse().unwrap()).unwrap();

    // Create owner account — use NEAR_OWNER_SECRET_KEY from env if set, else generate
    let owner_secret = match std::env::var("NEAR_OWNER_SECRET_KEY") {
        Ok(key) => key.parse().expect("invalid NEAR_OWNER_SECRET_KEY"),
        Err(_) => near_api::signer::generate_secret_key().unwrap(),
    };
    let owner_id: AccountId = "owner.sandbox".parse().unwrap();

    near_api::Account::create_account(owner_id.clone())
        .fund_myself(genesis_id.clone(), near_api::NearToken::from_near(100))
        .with_public_key(owner_secret.public_key())
        .with_signer(genesis_signer.clone())
        .send_to(&network)
        .await
        .unwrap()
        .assert_success();

    // Capture key strings for the /near-credentials endpoint
    let owner_public_key = owner_secret.public_key().to_string();
    let owner_secret_key = owner_secret.to_string();

    info!("Owner account: {}", owner_id);
    info!("Owner public key: {}", owner_public_key);

    let owner_signer = Signer::from_secret_key(owner_secret).unwrap();

    // Deploy git-storage WASM as a global contract
    let global_secret = near_api::signer::generate_secret_key().unwrap();
    let global_id: AccountId = "gitglobal.sandbox".parse().unwrap();

    near_api::Account::create_account(global_id.clone())
        .fund_myself(genesis_id.clone(), near_api::NearToken::from_near(50))
        .with_public_key(global_secret.public_key())
        .with_signer(genesis_signer.clone())
        .send_to(&network)
        .await
        .unwrap()
        .assert_success();

    let storage_wasm = std::fs::read("res/near_git_storage.wasm")
        .expect("Contract WASM not found. Run `./build.sh` first.");

    // Compute SHA-256 hash for hash-based global contract
    use sha2::{Digest, Sha256};
    let wasm_hash: String = Sha256::digest(&storage_wasm)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();

    Contract::deploy_global_contract_code(storage_wasm)
        .as_hash()
        .with_signer(global_id.clone(), Signer::from_secret_key(global_secret).unwrap())
        .send_to(&network)
        .await
        .unwrap()
        .assert_success();

    info!("Global contract deployed at {} (hash: {})", global_id, wasm_hash);

    // Deploy factory contract
    let factory_secret = near_api::signer::generate_secret_key().unwrap();
    let factory_id: AccountId = "factory.sandbox".parse().unwrap();

    near_api::Account::create_account(factory_id.clone())
        .fund_myself(genesis_id.clone(), near_api::NearToken::from_near(50))
        .with_public_key(factory_secret.public_key())
        .with_signer(genesis_signer.clone())
        .send_to(&network)
        .await
        .unwrap()
        .assert_success();

    let factory_wasm = std::fs::read("res/near_git_factory.wasm")
        .expect("Factory WASM not found. Run `./build.sh` first.");

    Contract::deploy(factory_id.clone())
        .use_code(factory_wasm)
        .with_init_call("new", json!({ "global_contract_hash": wasm_hash }))
        .unwrap()
        .with_signer(Signer::from_secret_key(factory_secret).unwrap())
        .send_to(&network)
        .await
        .unwrap()
        .assert_success();

    info!("Factory deployed at {}", factory_id);

    // Create repo via factory
    let contract_id: AccountId = "repo.factory.sandbox".parse().unwrap();

    Contract(factory_id.clone())
        .call_function("create_repo", json!({ "repo_name": "repo" }))
        .transaction()
        .deposit(near_api::NearToken::from_near(5))
        .with_signer(owner_id.clone(), owner_signer.clone())
        .send_to(&network)
        .await
        .unwrap()
        .assert_success();

    info!("Repo created at {}", contract_id);

    let state = Arc::new(AppState {
        sandbox,
        network,
        contract_id,
        owner_id,
        owner_signer,
        owner_public_key,
        owner_secret_key,
    });

    // CORS layer for browser-based wasm-git access
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any)
        .expose_headers(Any);

    let app = Router::new()
        .route("/{repo}/info/refs", get(handle_info_refs))
        .route("/{repo}/git-receive-pack", post(handle_receive_pack))
        .route("/{repo}/git-upload-pack", post(handle_upload_pack))
        .route("/near-call", post(handle_near_call))
        .route("/near-info", get(handle_near_info))
        .route("/near-credentials", get(handle_near_credentials))
        .route("/near-rpc", post(handle_near_rpc))
        .route("/parse-packfile", post(handle_parse_packfile))
        .layer(cors)
        .with_state(state);

    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    info!("Git server listening on http://{}", addr);
    info!("Clone URL: http://{}/repo", addr);
    info!("");
    info!("Usage:");
    info!("  git clone http://{}/repo", addr);
    info!("  cd repo && echo 'hello' > file.txt && git add . && git commit -m 'init'");
    info!("  git push origin main");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

/// GET /<repo>/info/refs?service=git-upload-pack|git-receive-pack
async fn handle_info_refs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<InfoRefsQuery>,
) -> Response {
    let service = &query.service;
    info!("info/refs service={}", service);

    // Fetch refs from contract
    let refs: Vec<(String, String)> = Contract(state.contract_id.clone())
        .call_function("get_refs", json!({}))
        .read_only()
        .fetch_from(&state.network)
        .await
        .unwrap()
        .data;

    let mut body = Vec::new();

    // Service announcement
    let announcement = format!("# service={}\n", service);
    body.extend_from_slice(&pktline::encode(announcement.as_bytes()));
    body.extend_from_slice(&pktline::flush());

    if refs.is_empty() {
        // Empty repo: advertise zero SHA with capabilities
        let line = format!(
            "{} capabilities^{{}}\0{}\n",
            ZERO_SHA, CAPABILITIES
        );
        body.extend_from_slice(&pktline::encode(line.as_bytes()));
    } else {
        // Determine HEAD: prefer refs/heads/main, then refs/heads/master, then first ref
        let head_sha = refs
            .iter()
            .find(|(name, _)| name == "refs/heads/main")
            .or_else(|| refs.iter().find(|(name, _)| name == "refs/heads/master"))
            .or_else(|| refs.first())
            .map(|(name, sha)| (name.clone(), sha.clone()));

        let (head_ref, head_sha_val) = head_sha.unwrap();
        let caps = format!("{} symref=HEAD:{}", CAPABILITIES, head_ref);

        // HEAD must be the first ref advertised (carries capabilities)
        let line = format!("{} HEAD\0{}\n", head_sha_val, caps);
        body.extend_from_slice(&pktline::encode(line.as_bytes()));

        for (ref_name, sha) in &refs {
            let line = format!("{} {}\n", sha, ref_name);
            body.extend_from_slice(&pktline::encode(line.as_bytes()));
        }
    }
    body.extend_from_slice(&pktline::flush());

    let content_type = format!("application/x-{}-advertisement", service);

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", content_type.parse().unwrap());
    headers.insert("Cache-Control", "no-cache".parse().unwrap());

    (StatusCode::OK, headers, body).into_response()
}

/// Manually borsh-serialize push args (pack_data + ref_updates).
fn encode_push_args(pack_data: &[u8], ref_updates: &[RefUpdate]) -> Vec<u8> {
    use borsh::BorshSerialize;
    let mut buf = Vec::new();
    pack_data.to_vec().serialize(&mut buf).unwrap();
    ref_updates.to_vec().serialize(&mut buf).unwrap();
    buf
}

/// POST /<repo>/git-receive-pack (push)
async fn handle_receive_pack(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Response {
    info!("git-receive-pack: {} bytes", body.len());

    // Parse the request: ref update commands followed by packfile
    let (commands, pack_data) = pktline::read_until_flush(&body);

    // Parse ref update commands
    let mut ref_updates = Vec::new();
    let mut ref_names = Vec::new();
    for cmd in &commands {
        let line = String::from_utf8_lossy(cmd);
        let line = line.trim_end_matches('\n');
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() >= 3 {
            let old_sha = parts[0].split('\0').next().unwrap_or(parts[0]);
            let new_sha = parts[1];
            let ref_name = parts[2].split('\0').next().unwrap_or(parts[2]);

            ref_updates.push(RefUpdate {
                name: ref_name.to_string(),
                old_sha: if old_sha == ZERO_SHA { None } else { Some(old_sha.to_string()) },
                new_sha: new_sha.to_string(),
            });
            ref_names.push(ref_name.to_string());

            info!("  ref update: {} {} -> {}", ref_name, old_sha, new_sha);
        }
    }

    // Store the packfile and update refs in one call
    let push_result = Contract(state.contract_id.clone())
        .call_function_raw("push", encode_push_args(pack_data, &ref_updates))
        .transaction()
        .gas(near_api::NearGas::from_tgas(300))
        .with_signer(state.owner_id.clone(), state.owner_signer.clone())
        .send_to(&state.network)
        .await;

    match push_result {
        Ok(r) if r.is_success() => {
            info!("  push succeeded ({} bytes packfile)", pack_data.len());
            let mut status_lines = vec!["unpack ok".to_string()];
            for name in &ref_names {
                status_lines.push(format!("ok {}", name));
            }
            make_receive_pack_response(&status_lines)
        }
        Ok(r) => {
            let err = format!("{:?}", r.assert_failure());
            error!("push failed: {}", err);
            make_receive_pack_response(&[format!("ng unpack {}", err)])
        }
        Err(e) => {
            error!("push RPC failed: {}", e);
            make_receive_pack_response(&["ng unpack contract call failed".into()])
        }
    }
}

/// Build a receive-pack response with report-status lines.
fn make_receive_pack_response(status_lines: &[String]) -> Response {
    let mut body = Vec::new();
    for line in status_lines {
        body.extend_from_slice(&pktline::encode(format!("{}\n", line).as_bytes()));
    }
    body.extend_from_slice(&pktline::flush());

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        "application/x-git-receive-pack-result".parse().unwrap(),
    );

    (StatusCode::OK, headers, body).into_response()
}

/// POST /<repo>/git-upload-pack (fetch/clone)
async fn handle_upload_pack(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Response {
    info!("git-upload-pack: {} bytes", body.len());

    // Parse wants and haves from the request
    let (lines, _rest) = pktline::read_until_flush(&body);

    let mut wants: Vec<String> = Vec::new();
    let mut haves: Vec<String> = Vec::new();

    for line in &lines {
        let text = String::from_utf8_lossy(line);
        let text = text.trim();
        if text.starts_with("want ") {
            // "want <sha> [capabilities]"
            let sha = text[5..].split_whitespace().next().unwrap_or("");
            if !sha.is_empty() {
                wants.push(sha.to_string());
            }
        } else if text.starts_with("have ") {
            let sha = text[5..].split_whitespace().next().unwrap_or("");
            if !sha.is_empty() {
                haves.push(sha.to_string());
            }
        } else if text == "done" {
            // Client is done sending haves
        }
    }

    // Also check for "done" after the flush (some clients send it in a second section)
    // Read remaining pkt-lines after the first flush
    if !body.is_empty() {
        let (after_lines, _) = pktline::read_until_flush(
            &body[body.len().saturating_sub(8)..], // quick check for trailing "done"
        );
        for line in &after_lines {
            let text = String::from_utf8_lossy(line);
            if text.trim().starts_with("have ") {
                let sha = text.trim()[5..].split_whitespace().next().unwrap_or("");
                if !sha.is_empty() {
                    haves.push(sha.to_string());
                }
            }
        }
    }

    info!("  wants: {:?}", wants);
    info!("  haves: {:?}", haves);

    if wants.is_empty() {
        // Nothing wanted
        let mut body = Vec::new();
        body.extend_from_slice(&pktline::encode(b"NAK\n"));
        body.extend_from_slice(&pktline::flush());

        let mut headers = HeaderMap::new();
        headers.insert(
            "Content-Type",
            "application/x-git-upload-pack-result".parse().unwrap(),
        );
        return (StatusCode::OK, headers, body).into_response();
    }

    // Get all packfiles from contract and concatenate them into a response
    let packs: Vec<Vec<u8>> = Contract(state.contract_id.clone())
        .call_function_borsh("get_packs", &0u32)
        .read_only_borsh()
        .fetch_from(&state.network)
        .await
        .unwrap()
        .data;

    info!("  {} packs from contract", packs.len());

    // Merge all packs into one by parsing objects and rebuilding
    let mut all_objects = Vec::new();
    for pack in &packs {
        if let Ok(result) = packfile::parse(pack) {
            for obj in result.objects {
                all_objects.push(obj);
            }
            // Resolve deltas using objects from the same pack
            let mut local: std::collections::HashMap<String, (String, Vec<u8>)> =
                std::collections::HashMap::new();
            for obj in &all_objects {
                local.insert(obj.sha1(), (obj.obj_type.clone(), obj.data.clone()));
            }
            for delta in &result.deltas {
                if let Some((obj_type, base_data)) = local.get(&delta.base_sha) {
                    if let Ok(resolved) = packfile::apply_delta(base_data, &delta.delta_data) {
                        let obj = packfile::PackObject { obj_type: obj_type.clone(), data: resolved };
                        local.insert(obj.sha1(), (obj.obj_type.clone(), obj.data.clone()));
                        all_objects.push(obj);
                    }
                }
            }
        }
    }

    let pack_data = packfile::build(&all_objects);
    info!("  built packfile: {} bytes, {} objects", pack_data.len(), all_objects.len());

    let mut response_body = Vec::new();
    response_body.extend_from_slice(&pktline::encode(b"NAK\n"));
    response_body.extend_from_slice(&pack_data);

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        "application/x-git-upload-pack-result".parse().unwrap(),
    );

    (StatusCode::OK, headers, response_body).into_response()
}

/// GET /near-info — return sandbox RPC URL and contract info for the service worker
async fn handle_near_info(State(state): State<Arc<AppState>>) -> Response {
    let rpc_url = state.network.rpc_endpoints.first()
        .map(|e| e.url.to_string())
        .unwrap_or_default();
    let info = json!({
        "rpcUrl": rpc_url,
        "contractId": state.contract_id.to_string(),
    });
    axum::Json(info).into_response()
}

/// GET /near-credentials — return owner credentials for service worker signing
async fn handle_near_credentials(State(state): State<Arc<AppState>>) -> Response {
    let rpc_url = state.network.rpc_endpoints.first()
        .map(|e| e.url.to_string())
        .unwrap_or_default();
    axum::Json(json!({
        "rpcUrl": rpc_url,
        "contractId": state.contract_id.to_string(),
        "accountId": state.owner_id.to_string(),
        "publicKey": state.owner_public_key,
        "secretKey": state.owner_secret_key,
    }))
    .into_response()
}

/// POST /near-call — proxy signed contract function calls from the service worker.
///
/// For push: accepts JSON with base64-encoded pack_data + ref_updates,
/// converts to borsh for the contract.
/// For other methods: passes JSON through directly.
async fn handle_near_call(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Response {
    let request: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return axum::Json(json!({ "success": false, "error": e.to_string() })).into_response();
        }
    };

    let method = request["method"].as_str().unwrap_or("");
    let args = &request["args"];

    info!("near-call: method={}", method);

    if method == "push" {
        // Decode pack_data from base64
        let pack_b64 = args["pack_data"].as_str().unwrap_or("");
        let pack_data = base64::engine::general_purpose::STANDARD
            .decode(pack_b64)
            .unwrap_or_default();

        let ref_updates: Vec<RefUpdate> = args["ref_updates"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|u| RefUpdate {
                        name: u["name"].as_str().unwrap_or("").to_string(),
                        old_sha: u["old_sha"].as_str().map(String::from),
                        new_sha: u["new_sha"].as_str().unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let result = Contract(state.contract_id.clone())
            .call_function_raw("push", encode_push_args(&pack_data, &ref_updates))
            .transaction()
            .gas(near_api::NearGas::from_tgas(300))
            .with_signer(state.owner_id.clone(), state.owner_signer.clone())
            .send_to(&state.network)
            .await;

        match result {
            Ok(r) => {
                let tx_hash = r.transaction().get_hash().to_string();
                if r.is_success() {
                    axum::Json(json!({ "success": true, "txHash": tx_hash })).into_response()
                } else {
                    axum::Json(json!({ "success": false, "error": "push failed", "txHash": tx_hash })).into_response()
                }
            }
            Err(e) => axum::Json(json!({ "success": false, "error": e.to_string() })).into_response(),
        }
    } else {
        // Other methods: pass JSON through directly
        let result = Contract(state.contract_id.clone())
            .call_function(method, args.clone())
            .transaction()
            .with_signer(state.owner_id.clone(), state.owner_signer.clone())
            .send_to(&state.network)
            .await;

        match result {
            Ok(r) => {
                let tx_hash = r.transaction().get_hash().to_string();
                if r.is_success() {
                    let full = r.into_full().unwrap();
                    let data: serde_json::Value = full.json().unwrap_or(json!(null));
                    axum::Json(json!({ "success": true, "result": data, "txHash": tx_hash })).into_response()
                } else {
                    axum::Json(json!({ "success": false, "error": "transaction failed", "txHash": tx_hash })).into_response()
                }
            }
            Err(e) => axum::Json(json!({ "success": false, "error": e.to_string() })).into_response(),
        }
    }
}

/// POST /near-rpc — proxy JSON-RPC requests to the sandbox's NEAR RPC
async fn handle_near_rpc(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Response {
    let rpc_url = state.network.rpc_endpoints.first()
        .map(|e| e.url.to_string())
        .unwrap_or_default();

    let client = reqwest::Client::new();
    match client.post(&rpc_url)
        .header("Content-Type", "application/json")
        .body(body.to_vec())
        .send()
        .await
    {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let body_bytes = resp.bytes().await.unwrap_or_default();
            let mut headers = HeaderMap::new();
            headers.insert("Content-Type", HeaderValue::from_static("application/json"));
            (status, headers, body_bytes.to_vec()).into_response()
        }
        Err(e) => {
            (StatusCode::BAD_GATEWAY, format!("RPC proxy error: {}", e)).into_response()
        }
    }
}

/// POST /parse-packfile — parse a raw packfile and return objects as JSON
async fn handle_parse_packfile(body: Bytes) -> Response {
    let parse_result = match packfile::parse(&body) {
        Ok(r) => r,
        Err(e) => {
            return axum::Json(json!({ "error": e })).into_response();
        }
    };

    let objects: Vec<serde_json::Value> = parse_result
        .objects
        .iter()
        .map(|obj| {
            json!({
                "obj_type": obj.obj_type,
                "data": base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &obj.data
                ),
            })
        })
        .collect();

    let deltas: Vec<serde_json::Value> = parse_result
        .deltas
        .iter()
        .map(|d| {
            json!({
                "base_sha": d.base_sha,
                "delta_data": base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &d.delta_data
                ),
            })
        })
        .collect();

    axum::Json(json!({ "objects": objects, "deltas": deltas })).into_response()
}
