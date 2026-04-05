/// git-remote-near: A git remote helper for NEAR blockchain storage.
///
/// Usage:
///   git clone near://<contract-id>
///   git remote add origin near://<contract-id>
///   git push origin main
///
/// Reads credentials from ~/.near-credentials/testnet/<signer>.json
/// or from the NEAR_SIGNER_ACCOUNT / NEAR_SIGNER_KEY env vars.
///
/// Configuration via git config or env:
///   NEAR_RPC_URL     — RPC endpoint (default: https://archival-rpc.testnet.fastnear.com)
///   NEAR_SIGNER_ACCOUNT — signer account ID (default: same as contract ID)
///   NEAR_SIGNER_KEY  — ed25519:<base58> private key (overrides credentials file)
///   NEAR_ENV         — "testnet" or "mainnet" (default: testnet)
use std::io::{self, BufRead, Write};
use std::sync::Arc;

use base64::Engine;
use near_api::{AccountId, Contract, Signer};
use serde_json::json;

use git_core::packfile;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // git calls: git-remote-near <remote-name> <url>
    if args.len() < 3 {
        eprintln!("Usage: git-remote-near <remote> <url>");
        std::process::exit(1);
    }

    let url = &args[2];
    // Parse near://<contract-id> or near::<contract-id>
    let contract_id_str = url
        .strip_prefix("near://")
        .or_else(|| url.strip_prefix("near::"))
        .unwrap_or(url);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(run(contract_id_str));
}

async fn run(contract_id_str: &str) {
    let contract_id: AccountId = contract_id_str.parse().expect("invalid contract ID");
    let network = resolve_network();

    // Lazy-load signer credentials (only needed for push)
    let mut signer_cache: Option<(AccountId, Arc<Signer>)> = None;

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    while let Some(Ok(line)) = lines.next() {
        let line = line.trim().to_string();

        if line.is_empty() {
            continue;
        }

        if line == "capabilities" {
            write!(out, "fetch\npush\n\n").unwrap();
            out.flush().unwrap();
        } else if line == "list" || line == "list for-push" {
            let refs = list_refs(&contract_id, &network).await;
            for (sha, name) in &refs {
                write!(out, "{} {}\n", sha, name).unwrap();
            }
            if refs.iter().any(|(_, n)| n == "refs/heads/main") {
                write!(out, "@refs/heads/main HEAD\n").unwrap();
            } else if refs.iter().any(|(_, n)| n == "refs/heads/master") {
                write!(out, "@refs/heads/master HEAD\n").unwrap();
            }
            write!(out, "\n").unwrap();
            out.flush().unwrap();
        } else if line.starts_with("fetch ") {
            let mut wants: Vec<String> = Vec::new();
            let sha = line.split_whitespace().nth(1).unwrap_or("").to_string();
            if !sha.is_empty() {
                wants.push(sha);
            }
            // Read remaining fetch lines until blank
            while let Some(Ok(next_line)) = lines.next() {
                let next_line = next_line.trim().to_string();
                if next_line.is_empty() {
                    break;
                }
                if next_line.starts_with("fetch ") {
                    let sha = next_line.split_whitespace().nth(1).unwrap_or("").to_string();
                    if !sha.is_empty() {
                        wants.push(sha);
                    }
                }
            }

            do_fetch(&wants, &contract_id, &network).await;
            write!(out, "\n").unwrap();
            out.flush().unwrap();
        } else if line.starts_with("push ") {
            let (signer_id, signer) = if let Some(ref cached) = signer_cache {
                cached.clone()
            } else {
                let (_, sid, s) = resolve_credentials(contract_id_str, &network).await;
                signer_cache = Some((sid.clone(), s.clone()));
                (sid, s)
            };

            let mut push_specs: Vec<String> = vec![line.clone()];
            while let Some(Ok(next_line)) = lines.next() {
                let next_line = next_line.trim().to_string();
                if next_line.is_empty() {
                    break;
                }
                push_specs.push(next_line);
            }

            let results = do_push(
                &push_specs,
                &contract_id,
                &signer_id,
                &signer,
                &network,
            )
            .await;
            for result in &results {
                write!(out, "{}\n", result).unwrap();
            }
            write!(out, "\n").unwrap();
            out.flush().unwrap();
        } else if line.starts_with("option ") {
            write!(out, "unsupported\n").unwrap();
            out.flush().unwrap();
        } else {
            eprintln!("git-remote-near: unknown command: {}", line);
        }
    }
}

fn resolve_network() -> near_api::NetworkConfig {
    let env = std::env::var("NEAR_ENV").unwrap_or_else(|_| "testnet".to_string());
    let default_rpc = match env.as_str() {
        "mainnet" => "https://archival-rpc.mainnet.fastnear.com",
        _ => "https://archival-rpc.testnet.fastnear.com",
    };
    let rpc_url = std::env::var("NEAR_RPC_URL").unwrap_or_else(|_| default_rpc.to_string());
    near_api::NetworkConfig::from_rpc_url(&env, rpc_url.parse().unwrap())
}

async fn resolve_credentials(contract_id_str: &str, network: &near_api::NetworkConfig) -> (AccountId, AccountId, Arc<Signer>) {
    let contract_id: AccountId = contract_id_str.parse().expect("invalid contract ID");

    // Check for signer@contract format
    let (signer_account, _contract) = if contract_id_str.contains('@') {
        let parts: Vec<&str> = contract_id_str.splitn(2, '@').collect();
        (parts[0].to_string(), parts[1].to_string())
    } else if let Ok(signer) = std::env::var("NEAR_SIGNER_ACCOUNT") {
        (signer, contract_id_str.to_string())
    } else {
        // Query the contract's owner to use as default signer
        let owner = Contract(contract_id.clone())
            .call_function("get_owner", json!({}))
            .read_only::<String>()
            .fetch_from(network)
            .await
            .map(|r| r.data)
            .unwrap_or_else(|_| contract_id_str.to_string());
        eprintln!("git-remote-near: using owner '{}' as signer", owner);
        (owner, contract_id_str.to_string())
    };
    let signer_id: AccountId = signer_account.parse().expect("invalid signer account");

    // Try env var first
    if let Ok(key) = std::env::var("NEAR_SIGNER_KEY") {
        let secret_key = key.parse().expect("invalid NEAR_SIGNER_KEY");
        let signer = Signer::from_secret_key(secret_key).unwrap();
        return (contract_id, signer_id, signer);
    }

    // Try ~/.near-credentials/<env>/<account>.json
    let env = std::env::var("NEAR_ENV").unwrap_or_else(|_| "testnet".to_string());
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let cred_path = format!(
        "{}/.near-credentials/{}/{}.json",
        home, env, signer_account
    );

    let cred_data = std::fs::read_to_string(&cred_path)
        .unwrap_or_else(|_| panic!(
            "No credentials found. Set NEAR_SIGNER_KEY or create {}", cred_path
        ));
    let cred: serde_json::Value = serde_json::from_str(&cred_data)
        .unwrap_or_else(|_| panic!("Invalid JSON in {}", cred_path));

    let private_key = cred["private_key"]
        .as_str()
        .expect("no private_key in credentials file");

    let secret_key = private_key.parse().expect("invalid private key in credentials file");
    let signer = Signer::from_secret_key(secret_key).unwrap();

    (contract_id, signer_id, signer)
}

async fn list_refs(
    contract_id: &AccountId,
    network: &near_api::NetworkConfig,
) -> Vec<(String, String)> {
    let result: Vec<(String, String)> = Contract(contract_id.clone())
        .call_function("get_refs", json!({}))
        .read_only()
        .fetch_from(network)
        .await
        .unwrap()
        .data;
    // Contract returns (refname, sha), we need (sha, refname)
    result.into_iter().map(|(name, sha)| (sha, name)).collect()
}

/// Fetch objects from archival RPC by transaction hash.
/// Extracts push_objects args from the transaction to recover git objects.
async fn fetch_objects_from_tx(
    tx_hash: &str,
    signer_id: &str,
    rpc_url: &str,
) -> Vec<packfile::PackObject> {
    let client = reqwest::Client::new();
    let resp = client
        .post(rpc_url)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "EXPERIMENTAL_tx_status",
            "params": {
                "tx_hash": tx_hash,
                "sender_account_id": signer_id,
                "wait_until": "EXECUTED",
            },
        }))
        .send()
        .await
        .expect("archival RPC request failed");

    let data: serde_json::Value = resp.json().await.expect("invalid RPC response");
    let tx = &data["result"]["transaction"];
    let actions = tx["actions"].as_array();

    let mut objects = Vec::new();
    if let Some(actions) = actions {
        for action in actions {
            if let Some(fc) = action.get("FunctionCall") {
                if fc["method_name"].as_str() == Some("push_objects") {
                    let args_b64 = fc["args"].as_str().unwrap_or("");
                    let args_bytes = base64::engine::general_purpose::STANDARD
                        .decode(args_b64)
                        .unwrap_or_default();
                    let args: serde_json::Value =
                        serde_json::from_slice(&args_bytes).unwrap_or(json!(null));

                    if let Some(objs) = args["objects"].as_array() {
                        for obj in objs {
                            let obj_type =
                                obj["obj_type"].as_str().unwrap_or("blob").to_string();
                            let data_b64 = obj["data"].as_str().unwrap_or("");
                            let obj_data = base64::engine::general_purpose::STANDARD
                                .decode(data_b64)
                                .unwrap_or_default();
                            objects.push(packfile::PackObject {
                                obj_type,
                                data: obj_data,
                            });
                        }
                    }
                }
            }
        }
    }
    objects
}

/// Extract child SHAs from a git object for graph walking.
fn extract_children(obj: &packfile::PackObject) -> Vec<String> {
    let mut children = Vec::new();
    match obj.obj_type.as_str() {
        "commit" => {
            let text = String::from_utf8_lossy(&obj.data);
            for line in text.lines() {
                if let Some(tree_sha) = line.strip_prefix("tree ") {
                    children.push(tree_sha.trim().to_string());
                } else if let Some(parent_sha) = line.strip_prefix("parent ") {
                    children.push(parent_sha.trim().to_string());
                } else if line.is_empty() {
                    break;
                }
            }
        }
        "tree" => {
            let mut pos = 0;
            while pos < obj.data.len() {
                let null_pos = obj.data[pos..]
                    .iter()
                    .position(|&b| b == 0)
                    .map(|p| pos + p);
                if let Some(null_pos) = null_pos {
                    if null_pos + 21 <= obj.data.len() {
                        let child_sha: String = obj.data[null_pos + 1..null_pos + 21]
                            .iter()
                            .map(|b| format!("{:02x}", b))
                            .collect();
                        children.push(child_sha);
                        pos = null_pos + 21;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        _ => {}
    }
    children
}

async fn do_fetch(
    wants: &[String],
    contract_id: &AccountId,
    network: &near_api::NetworkConfig,
) {
    let rpc_url = network.rpc_endpoints.first()
        .map(|e| e.url.to_string())
        .expect("no RPC endpoint configured");

    // Step 1: Walk the object graph to find all needed SHAs.
    // We fetch one SHA at a time, get its tx, extract objects, then walk children.
    let mut all_objects: Vec<packfile::PackObject> = Vec::new();
    let mut sha_to_obj: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut visited_txs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut visited_shas: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<String> = wants.iter().cloned().collect();

    while !queue.is_empty() {
        // Collect current batch of needed SHAs
        let mut batch: Vec<String> = Vec::new();
        while let Some(sha) = queue.pop_front() {
            if visited_shas.contains(&sha) {
                continue;
            }
            visited_shas.insert(sha.clone());
            batch.push(sha);
            if batch.len() >= 50 {
                break;
            }
        }
        if batch.is_empty() {
            continue;
        }

        // Get tx locations for this batch
        let locations: Vec<(String, Option<String>)> = Contract(contract_id.clone())
            .call_function("get_object_locations", json!({ "shas": &batch }))
            .read_only()
            .fetch_from(network)
            .await
            .unwrap()
            .data;

        // Fetch unique transactions and extract objects
        for (sha, maybe_tx) in &locations {
            if let Some(tx_hash) = maybe_tx {
                if visited_txs.contains(tx_hash) {
                    continue;
                }
                visited_txs.insert(tx_hash.clone());

                eprintln!("git-remote-near: fetching tx {}", tx_hash);
                let tx_objects = fetch_objects_from_tx(
                    tx_hash,
                    contract_id.as_ref(),
                    &rpc_url,
                ).await;

                // Index and walk children
                for obj in tx_objects {
                    let obj_sha = git_sha_of(&obj);
                    for child in extract_children(&obj) {
                        if !visited_shas.contains(&child) {
                            queue.push_back(child);
                        }
                    }
                    if !sha_to_obj.contains_key(&obj_sha) {
                        sha_to_obj.insert(obj_sha, all_objects.len());
                        all_objects.push(obj);
                    }
                }
            } else {
                eprintln!("git-remote-near: warning: no tx location for {}", sha);
            }
        }
    }

    eprintln!(
        "git-remote-near: fetched {} objects from {} transactions",
        all_objects.len(),
        visited_txs.len()
    );

    // Build packfile and feed to `git index-pack`
    let pack_data = packfile::build(&all_objects);
    eprintln!(
        "git-remote-near: indexing packfile ({} bytes, {} objects)",
        pack_data.len(),
        all_objects.len()
    );

    let mut child = std::process::Command::new("git")
        .args(["index-pack", "--stdin", "--fix-thin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to run git index-pack");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&pack_data)
        .unwrap();

    let output = child.wait_with_output().unwrap();
    if !output.status.success() {
        eprintln!(
            "git-remote-near: index-pack failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Compute git SHA-1 for a PackObject.
fn git_sha_of(obj: &packfile::PackObject) -> String {
    use sha1::{Digest, Sha1};
    let header = format!("{} {}\0", obj.obj_type, obj.data.len());
    let mut hasher = Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(&obj.data);
    hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

async fn do_push(
    push_specs: &[String],
    contract_id: &AccountId,
    signer_id: &AccountId,
    signer: &Arc<Signer>,
    network: &near_api::NetworkConfig,
) -> Vec<String> {
    let mut results = Vec::new();

    // Parse push specs: "push [+]<src>:<dst>"
    struct PushOp {
        _force: bool,
        src_ref: String,
        dst_ref: String,
    }

    let mut ops = Vec::new();
    for spec in push_specs {
        let spec = spec.strip_prefix("push ").unwrap_or(spec);
        let force = spec.starts_with('+');
        let spec = if force { &spec[1..] } else { spec };

        let parts: Vec<&str> = spec.splitn(2, ':').collect();
        if parts.len() == 2 {
            ops.push(PushOp {
                _force: force,
                src_ref: parts[0].to_string(),
                dst_ref: parts[1].to_string(),
            });
        }
    }

    // For each push, we need to:
    // 1. Figure out which objects are new (not on the remote)
    // 2. Send them to the contract
    // 3. Update the refs

    // Get current remote refs for comparison
    let remote_refs: Vec<(String, String)> = Contract(contract_id.clone())
        .call_function("get_refs", json!({}))
        .read_only()
        .fetch_from(network)
        .await
        .unwrap()
        .data;
    let remote_ref_map: std::collections::HashMap<String, String> =
        remote_refs.into_iter().collect();

    for op in &ops {
        if op.src_ref.is_empty() {
            // Delete ref
            results.push(format!("error {} delete not supported yet", op.dst_ref));
            continue;
        }

        // Resolve the local ref to a SHA
        let local_sha = resolve_local_ref(&op.src_ref);
        if local_sha.is_empty() {
            results.push(format!("error {} cannot resolve ref", op.dst_ref));
            continue;
        }

        let old_sha = remote_ref_map.get(&op.dst_ref).cloned();

        // Collect objects to push: walk from local_sha, stop at remote objects
        let remote_shas: std::collections::HashSet<String> = {
            let mut set = std::collections::HashSet::new();
            if let Some(old) = &old_sha {
                set.insert(old.clone());
                // Also collect all objects reachable from old to avoid re-pushing
                collect_reachable_local(old, &mut set);
            }
            set
        };

        let mut to_push: Vec<packfile::PackObject> = Vec::new();
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        queue.push_back(local_sha.clone());

        while let Some(sha) = queue.pop_front() {
            if visited.contains(&sha) || remote_shas.contains(&sha) {
                continue;
            }
            visited.insert(sha.clone());

            // Read from local git
            if let Some(obj) = read_local_object(&sha) {
                // Parse children for graph walking
                match obj.obj_type.as_str() {
                    "commit" => {
                        let text = String::from_utf8_lossy(&obj.data);
                        for line in text.lines() {
                            if let Some(tree_sha) = line.strip_prefix("tree ") {
                                queue.push_back(tree_sha.trim().to_string());
                            } else if let Some(parent_sha) = line.strip_prefix("parent ") {
                                queue.push_back(parent_sha.trim().to_string());
                            } else if line.is_empty() {
                                break;
                            }
                        }
                    }
                    "tree" => {
                        let mut pos = 0;
                        while pos < obj.data.len() {
                            let null_pos = obj.data[pos..]
                                .iter()
                                .position(|&b| b == 0)
                                .map(|p| pos + p);
                            if let Some(null_pos) = null_pos {
                                if null_pos + 21 <= obj.data.len() {
                                    let child_sha: String =
                                        obj.data[null_pos + 1..null_pos + 21]
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
                    _ => {}
                }
                to_push.push(obj);
            }
        }

        if to_push.is_empty() {
            // Nothing new to push, just update ref
            let ref_updates = vec![json!({
                "name": op.dst_ref,
                "old_sha": old_sha,
                "new_sha": local_sha,
            })];

            match Contract(contract_id.clone())
                .call_function("register_push", json!({
                    "tx_hash": "none",
                    "object_shas": [],
                    "ref_updates": ref_updates,
                }))
                .transaction()
                .with_signer(signer_id.clone(), signer.clone())
                .send_to(network)
                .await
            {
                Ok(r) if r.is_success() => {
                    results.push(format!("ok {}", op.dst_ref));
                }
                Ok(r) => {
                    results.push(format!("error {} {:?}", op.dst_ref, r.assert_failure()));
                }
                Err(e) => {
                    results.push(format!("error {} {}", op.dst_ref, e));
                }
            }
            continue;
        }

        eprintln!(
            "git-remote-near: pushing {} objects for {}",
            to_push.len(),
            op.dst_ref
        );

        // Push objects to contract
        let git_objects: Vec<serde_json::Value> = to_push
            .iter()
            .map(|obj| {
                json!({
                    "obj_type": obj.obj_type,
                    "data": base64::engine::general_purpose::STANDARD.encode(&obj.data),
                })
            })
            .collect();

        let push_result = Contract(contract_id.clone())
            .call_function("push_objects", json!({ "objects": git_objects }))
            .transaction()
            .with_signer(signer_id.clone(), signer.clone())
            .send_to(network)
            .await;

        let (tx_hash, object_shas) = match push_result {
            Ok(r) => {
                let tx_hash = r.transaction().get_hash().to_string();
                if !r.is_success() {
                    results.push(format!("error {} push_objects failed", op.dst_ref));
                    continue;
                }
                let full = r.into_full().unwrap();
                let data: serde_json::Value = full.json().unwrap_or(json!(null));
                let shas: Vec<String> = data["shas"]
                    .as_array()
                    .unwrap_or(&Vec::new())
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                (tx_hash, shas)
            }
            Err(e) => {
                results.push(format!("error {} {}", op.dst_ref, e));
                continue;
            }
        };

        eprintln!(
            "git-remote-near: pushed {} objects (tx={})",
            object_shas.len(),
            tx_hash
        );

        // Register push and update refs
        let ref_updates = vec![json!({
            "name": op.dst_ref,
            "old_sha": old_sha,
            "new_sha": local_sha,
        })];

        match Contract(contract_id.clone())
            .call_function("register_push", json!({
                "tx_hash": tx_hash,
                "object_shas": object_shas,
                "ref_updates": ref_updates,
            }))
            .transaction()
            .with_signer(signer_id.clone(), signer.clone())
            .send_to(network)
            .await
        {
            Ok(r) if r.is_success() => {
                results.push(format!("ok {}", op.dst_ref));
            }
            Ok(r) => {
                results.push(format!("error {} {:?}", op.dst_ref, r.assert_failure()));
            }
            Err(e) => {
                results.push(format!("error {} {}", op.dst_ref, e));
            }
        }
    }

    results
}

/// Resolve a local git ref to a SHA using `git rev-parse`.
fn resolve_local_ref(refspec: &str) -> String {
    let output = std::process::Command::new("git")
        .args(["rev-parse", refspec])
        .output();
    match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

/// Read a git object from the local repo using `git cat-file`.
fn read_local_object(sha: &str) -> Option<packfile::PackObject> {
    // Get type
    let type_output = std::process::Command::new("git")
        .args(["cat-file", "-t", sha])
        .output()
        .ok()?;
    if !type_output.status.success() {
        return None;
    }
    let obj_type = String::from_utf8_lossy(&type_output.stdout)
        .trim()
        .to_string();

    // Get data
    let data_output = std::process::Command::new("git")
        .args(["cat-file", obj_type.as_str(), sha])
        .output()
        .ok()?;
    if !data_output.status.success() {
        return None;
    }

    Some(packfile::PackObject {
        obj_type,
        data: data_output.stdout,
    })
}

/// Collect all SHAs reachable from a given SHA in the local repo.
fn collect_reachable_local(sha: &str, set: &mut std::collections::HashSet<String>) {
    let output = std::process::Command::new("git")
        .args(["rev-list", "--objects", sha])
        .output();
    if let Ok(o) = output {
        if o.status.success() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let obj_sha = line.split_whitespace().next().unwrap_or("");
                if !obj_sha.is_empty() {
                    set.insert(obj_sha.to_string());
                }
            }
        }
    }
}
