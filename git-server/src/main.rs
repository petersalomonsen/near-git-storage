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
use near_api::{AccountId, Contract, Signer};
use near_sandbox::Sandbox;
use serde::Deserialize;
use serde_json::json;
use tracing::{error, info};

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

    // Create contract account and deploy
    let contract_secret = near_api::signer::generate_secret_key().unwrap();
    let contract_id: AccountId = "repo.sandbox".parse().unwrap();

    near_api::Account::create_account(contract_id.clone())
        .fund_myself(genesis_id.clone(), near_api::NearToken::from_near(100))
        .with_public_key(contract_secret.public_key())
        .with_signer(genesis_signer)
        .send_to(&network)
        .await
        .unwrap()
        .assert_success();

    let contract_signer = Signer::from_secret_key(contract_secret).unwrap();

    let wasm = std::fs::read("res/near_git_storage.wasm")
        .expect("Contract WASM not found. Run `./build.sh` first.");

    Contract::deploy(contract_id.clone())
        .use_code(wasm)
        .with_init_call("new", json!({ "owner": owner_id.to_string() }))
        .unwrap()
        .with_signer(contract_signer)
        .send_to(&network)
        .await
        .unwrap()
        .assert_success();

    info!("Contract deployed at {}", contract_id);

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
        .route("/parse-packfile", post(handle_parse_packfile))
        .layer(cors)
        .with_state(state);

    let addr = "127.0.0.1:8080";
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

/// POST /<repo>/git-receive-pack (push)
async fn handle_receive_pack(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Response {
    info!("git-receive-pack: {} bytes", body.len());

    // Parse the request: ref update commands followed by packfile
    let (commands, rest) = pktline::read_until_flush(&body);

    // Parse ref update commands
    let mut ref_updates = Vec::new();
    for cmd in &commands {
        let line = String::from_utf8_lossy(cmd);
        // Strip trailing newline and capabilities
        let line = line.trim_end_matches('\n');
        let parts: Vec<&str> = line.splitn(3, ' ').collect();
        if parts.len() >= 3 {
            let old_sha = parts[0].split('\0').next().unwrap_or(parts[0]);
            let new_sha = parts[1];
            // ref name may have \0capabilities appended
            let ref_name = parts[2].split('\0').next().unwrap_or(parts[2]);

            let old = if old_sha == ZERO_SHA {
                None
            } else {
                Some(old_sha.to_string())
            };

            ref_updates.push(json!({
                "name": ref_name,
                "old_sha": old,
                "new_sha": new_sha,
            }));

            info!("  ref update: {} {} -> {}", ref_name, old_sha, new_sha);
        }
    }

    // Check if this is a delete-only push (new_sha is all zeros)
    let is_delete_only = ref_updates.iter().all(|u| {
        u["new_sha"].as_str() == Some(ZERO_SHA)
    });

    let mut object_shas = Vec::new();

    if !is_delete_only && !rest.is_empty() {
        // Parse packfile
        let parse_result = match packfile::parse(rest) {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to parse packfile: {}", e);
                return make_receive_pack_response(&[format!("ng unpack {}", e)]);
            }
        };

        let mut pack_objects = parse_result.objects;

        // Resolve ref_delta objects by fetching base objects from contract
        if !parse_result.deltas.is_empty() {
            info!(
                "  resolving {} ref_delta objects",
                parse_result.deltas.len()
            );

            // Build a map of objects we already have (from this pack)
            let mut local_objects: std::collections::HashMap<String, (String, Vec<u8>)> =
                std::collections::HashMap::new();
            for obj in &pack_objects {
                local_objects.insert(obj.sha1(), (obj.obj_type.clone(), obj.data.clone()));
            }

            for delta in &parse_result.deltas {
                // Try local first, then fetch from contract
                let base = if let Some((obj_type, data)) = local_objects.get(&delta.base_sha) {
                    Some((obj_type.clone(), data.clone()))
                } else {
                    // Fetch from contract
                    let result: Vec<(String, Option<serde_json::Value>)> =
                        Contract(state.contract_id.clone())
                            .call_function(
                                "get_objects",
                                json!({ "shas": [&delta.base_sha] }),
                            )
                            .read_only()
                            .fetch_from(&state.network)
                            .await
                            .unwrap()
                            .data;

                    result.into_iter().next().and_then(|(_, v)| {
                        v.map(|obj| {
                            let obj_type =
                                obj["obj_type"].as_str().unwrap_or("blob").to_string();
                            let data_b64 = obj["data"].as_str().unwrap_or("");
                            let data = base64::engine::general_purpose::STANDARD
                                .decode(data_b64)
                                .unwrap_or_default();
                            (obj_type, data)
                        })
                    })
                };

                match base {
                    Some((obj_type, base_data)) => {
                        match packfile::apply_delta(&base_data, &delta.delta_data) {
                            Ok(resolved_data) => {
                                pack_objects.push(packfile::PackObject {
                                    obj_type,
                                    data: resolved_data,
                                });
                            }
                            Err(e) => {
                                error!("Failed to apply delta: {}", e);
                                return make_receive_pack_response(&[format!(
                                    "ng unpack delta apply failed: {}",
                                    e
                                )]);
                            }
                        }
                    }
                    None => {
                        error!(
                            "Base object {} not found for delta",
                            delta.base_sha
                        );
                        return make_receive_pack_response(&[format!(
                            "ng unpack base object {} not found",
                            delta.base_sha
                        )]);
                    }
                }
            }
        }

        info!("  parsed {} objects from packfile", pack_objects.len());

        // Convert to contract format and push
        let git_objects: Vec<serde_json::Value> = pack_objects
            .iter()
            .map(|obj| {
                json!({
                    "obj_type": obj.obj_type,
                    "data": base64::engine::general_purpose::STANDARD.encode(&obj.data),
                })
            })
            .collect();

        // Call push_objects on the contract
        let push_result = Contract(state.contract_id.clone())
            .call_function("push_objects", json!({ "objects": git_objects }))
            .transaction()
            .with_signer(state.owner_id.clone(), state.owner_signer.clone())
            .send_to(&state.network)
            .await;

        let push_result = match push_result {
            Ok(r) => r,
            Err(e) => {
                error!("push_objects RPC failed: {}", e);
                return make_receive_pack_response(&["ng unpack contract call failed".into()]);
            }
        };

        let full = match push_result.into_full() {
            Some(f) => f,
            None => {
                return make_receive_pack_response(&["ng unpack transaction pending".into()]);
            }
        };

        let tx_hash = full.outcome().transaction_hash.to_string();

        let push_data: serde_json::Value = match full.json() {
            Ok(v) => v,
            Err(e) => {
                error!("push_objects failed: {}", e);
                return make_receive_pack_response(&[format!("ng unpack {}", e)]);
            }
        };

        object_shas = push_data["shas"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        info!("  pushed {} objects, tx={}", object_shas.len(), tx_hash);

        // Register push with ref updates
        let register_result = Contract(state.contract_id.clone())
            .call_function(
                "register_push",
                json!({
                    "tx_hash": tx_hash,
                    "object_shas": object_shas,
                    "ref_updates": ref_updates,
                }),
            )
            .transaction()
            .with_signer(state.owner_id.clone(), state.owner_signer.clone())
            .send_to(&state.network)
            .await;

        match register_result {
            Ok(r) if r.is_success() => {
                info!("  refs updated successfully");
            }
            Ok(r) => {
                let err = format!("{:?}", r.assert_failure());
                error!("register_push failed: {}", err);
                return make_receive_pack_response(&[format!("ng refs {}", err)]);
            }
            Err(e) => {
                error!("register_push RPC failed: {}", e);
                return make_receive_pack_response(&["ng refs contract call failed".into()]);
            }
        }
    } else if !is_delete_only {
        // No packfile but not a delete — just update refs
        let register_result = Contract(state.contract_id.clone())
            .call_function(
                "register_push",
                json!({
                    "tx_hash": "none",
                    "object_shas": [],
                    "ref_updates": ref_updates,
                }),
            )
            .transaction()
            .with_signer(state.owner_id.clone(), state.owner_signer.clone())
            .send_to(&state.network)
            .await;

        match register_result {
            Ok(r) if r.is_success() => {}
            _ => {
                return make_receive_pack_response(&["ng refs update failed".into()]);
            }
        }
    }

    // Build success response with per-ref status
    let mut status_lines = vec!["unpack ok".to_string()];
    for update in &ref_updates {
        if let Some(ref_name) = update["name"].as_str() {
            status_lines.push(format!("ok {}", ref_name));
        }
    }
    make_receive_pack_response(&status_lines)
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

    // Walk the object graph from wants, stopping at haves
    let haves_set: std::collections::HashSet<String> = haves.into_iter().collect();
    let mut needed: Vec<String> = Vec::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<String> = wants.into_iter().collect();

    while let Some(sha) = queue.pop_front() {
        if visited.contains(&sha) || haves_set.contains(&sha) {
            continue;
        }
        visited.insert(sha.clone());
        needed.push(sha.clone());

        // Fetch this object to find children
        let objects: Vec<(String, Option<serde_json::Value>)> =
            Contract(state.contract_id.clone())
                .call_function("get_objects", json!({ "shas": [&sha] }))
                .read_only()
                .fetch_from(&state.network)
                .await
                .unwrap()
                .data;

        for (_sha, maybe_obj) in objects {
            if let Some(obj) = maybe_obj {
                let obj_type = obj["obj_type"].as_str().unwrap_or("");
                let data_b64 = obj["data"].as_str().unwrap_or("");
                let data = base64::engine::general_purpose::STANDARD
                    .decode(data_b64)
                    .unwrap_or_default();

                // Parse children depending on type
                match obj_type {
                    "commit" => {
                        // Parse commit to find tree and parent SHAs
                        let text = String::from_utf8_lossy(&data);
                        for line in text.lines() {
                            if let Some(tree_sha) = line.strip_prefix("tree ") {
                                queue.push_back(tree_sha.trim().to_string());
                            } else if let Some(parent_sha) = line.strip_prefix("parent ") {
                                queue.push_back(parent_sha.trim().to_string());
                            } else if line.is_empty() {
                                break; // End of headers
                            }
                        }
                    }
                    "tree" => {
                        // Parse tree entries: <mode> <name>\0<20-byte-sha>
                        let mut pos = 0;
                        while pos < data.len() {
                            // Find the null byte
                            let null_pos = data[pos..]
                                .iter()
                                .position(|&b| b == 0)
                                .map(|p| pos + p);
                            if let Some(null_pos) = null_pos {
                                if null_pos + 21 <= data.len() {
                                    let child_sha_bytes = &data[null_pos + 1..null_pos + 21];
                                    let child_sha: String = child_sha_bytes
                                        .iter()
                                        .map(|b| format!("{:02x}", b))
                                        .collect();
                                    queue.push_back(child_sha);
                                    pos = null_pos + 21;
                                } else {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                    }
                    _ => {} // blob, tag — no children to walk
                }
            }
        }
    }

    info!("  need {} objects total", needed.len());

    // Fetch all needed objects in batches
    let mut pack_objects = Vec::new();

    for chunk in needed.chunks(50) {
        let objects: Vec<(String, Option<serde_json::Value>)> =
            Contract(state.contract_id.clone())
                .call_function("get_objects", json!({ "shas": chunk }))
                .read_only()
                .fetch_from(&state.network)
                .await
                .unwrap()
                .data;

        for (_sha, maybe_obj) in objects {
            if let Some(obj) = maybe_obj {
                let obj_type = obj["obj_type"].as_str().unwrap_or("blob").to_string();
                let data_b64 = obj["data"].as_str().unwrap_or("");
                let data = base64::engine::general_purpose::STANDARD
                    .decode(data_b64)
                    .unwrap_or_default();

                pack_objects.push(packfile::PackObject { obj_type, data });
            }
        }
    }

    // Build packfile
    let pack_data = packfile::build(&pack_objects);
    info!("  built packfile: {} bytes, {} objects", pack_data.len(), pack_objects.len());

    // Build response
    let mut response_body = Vec::new();
    response_body.extend_from_slice(&pktline::encode(b"NAK\n"));

    // Send packfile data using side-band-64k (channel 1 = pack data)
    // Actually, for simplicity, just send the packfile directly without sideband
    // The NAK followed by raw packfile data is the simplest protocol variant
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

/// POST /near-call — proxy signed contract function calls from the service worker
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
                axum::Json(json!({
                    "success": true,
                    "result": data,
                    "txHash": tx_hash,
                }))
                .into_response()
            } else {
                axum::Json(json!({
                    "success": false,
                    "error": "transaction failed",
                    "txHash": tx_hash,
                }))
                .into_response()
            }
        }
        Err(e) => {
            axum::Json(json!({ "success": false, "error": e.to_string() })).into_response()
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
