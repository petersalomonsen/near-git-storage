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
///   NEAR_RPC_URL     — RPC endpoint (default: https://rpc.testnet.fastnear.com)
///   NEAR_SIGNER_ACCOUNT — signer account ID (default: same as contract ID)
///   NEAR_SIGNER_KEY  — ed25519:<base58> private key (overrides credentials file)
///   NEAR_ENV         — "testnet" or "mainnet" (default: testnet)
use std::io::{self, BufRead, Write};
use std::sync::Arc;

use borsh::BorshSerialize;
use near_api::{AccountId, Contract, Signer};
use serde_json::json;

/// Borsh-serialized ref update for push calls.
#[derive(BorshSerialize, Clone)]
struct RefUpdate {
    name: String,
    old_sha: Option<String>,
    new_sha: String,
}

/// Manually borsh-serialize push args (two sequential fields).
fn encode_push_args(pack_data: &[u8], ref_updates: &[RefUpdate]) -> Vec<u8> {
    let mut buf = Vec::new();
    pack_data.serialize(&mut buf).unwrap();
    ref_updates.to_vec().serialize(&mut buf).unwrap();
    buf
}

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
        "mainnet" => "https://rpc.mainnet.fastnear.com",
        _ => "https://rpc.testnet.fastnear.com",
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

async fn do_fetch(
    _wants: &[String],
    contract_id: &AccountId,
    network: &near_api::NetworkConfig,
) {
    // Get all packfiles from the contract
    eprintln!("git-remote-near: fetching packfiles...");

    let packs: Vec<Vec<u8>> = Contract(contract_id.clone())
        .call_function_borsh("get_packs", &0u32)
        .read_only_borsh()
        .fetch_from(network)
        .await
        .unwrap()
        .data;

    eprintln!("git-remote-near: received {} packfiles", packs.len());

    // Feed each packfile to `git index-pack` to import into local repo
    for (i, pack_data) in packs.iter().enumerate() {
        eprintln!(
            "git-remote-near: indexing pack {}/{} ({} bytes)",
            i + 1,
            packs.len(),
            pack_data.len()
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
            .write_all(pack_data)
            .unwrap();

        let output = child.wait_with_output().unwrap();
        if !output.status.success() {
            eprintln!(
                "git-remote-near: index-pack failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
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
            results.push(format!("error {} delete not supported yet", op.dst_ref));
            continue;
        }

        let local_sha = resolve_local_ref(&op.src_ref);
        if local_sha.is_empty() {
            results.push(format!("error {} cannot resolve ref", op.dst_ref));
            continue;
        }

        let old_sha = remote_ref_map.get(&op.dst_ref).cloned();

        // Check if there are new objects to push
        let new_shas = collect_new_shas(&local_sha, &old_sha);

        if new_shas.is_empty() {
            // Nothing new, just update ref
            let ref_updates = vec![RefUpdate {
                name: op.dst_ref.clone(),
                old_sha: old_sha.clone(),
                new_sha: local_sha.clone(),
            }];
            let empty_pack: Vec<u8> = Vec::new();

            match Contract(contract_id.clone())
                .call_function_raw("push", encode_push_args(&empty_pack, &ref_updates))
                .transaction()
                .gas(near_api::NearGas::from_tgas(300))
                .with_signer(signer_id.clone(), signer.clone())
                .send_to(network)
                .await
            {
                Ok(r) if r.is_success() => results.push(format!("ok {}", op.dst_ref)),
                Ok(r) => results.push(format!("error {} {:?}", op.dst_ref, r.assert_failure())),
                Err(e) => results.push(format!("error {} {}", op.dst_ref, e)),
            }
            continue;
        }

        eprintln!(
            "git-remote-near: packing {} new objects for {}",
            new_shas.len(),
            op.dst_ref
        );

        // Build thin packfile with delta compression against existing remote objects
        let pack_data = build_packfile(&local_sha, &old_sha);

        eprintln!(
            "git-remote-near: packfile {} bytes (from {} objects, thin={})",
            pack_data.len(),
            new_shas.len(),
            old_sha.is_some()
        );

        // Send packfile + ref updates in batches if needed
        let ref_updates = vec![RefUpdate {
            name: op.dst_ref.clone(),
            old_sha: old_sha.clone(),
            new_sha: local_sha.clone(),
        }];

        match Contract(contract_id.clone())
            .call_function_raw("push", encode_push_args(&pack_data, &ref_updates))
            .transaction()
            .gas(near_api::NearGas::from_tgas(300))
            .with_signer(signer_id.clone(), signer.clone())
            .send_to(network)
            .await
        {
            Ok(r) if r.is_success() => results.push(format!("ok {}", op.dst_ref)),
            Ok(r) => results.push(format!("error {} {:?}", op.dst_ref, r.assert_failure())),
            Err(e) => results.push(format!("error {} {}", op.dst_ref, e)),
        }
    }

    results
}

/// Collect SHAs of new objects to push (local objects not in remote).
fn collect_new_shas(local_sha: &str, old_sha: &Option<String>) -> Vec<String> {
    // Use `git rev-list --objects <new> --not <old>` to get new objects
    let mut cmd_args = vec!["rev-list".to_string(), "--objects".to_string(), local_sha.to_string()];
    if let Some(old) = old_sha {
        cmd_args.push("--not".to_string());
        cmd_args.push(old.clone());
    }

    let output = std::process::Command::new("git")
        .args(&cmd_args)
        .output();

    match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|line| {
                    let sha = line.split_whitespace().next().unwrap_or("");
                    if sha.is_empty() { None } else { Some(sha.to_string()) }
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Build a packfile using `git pack-objects --revs --thin`.
/// `new_sha` is the new commit, `old_sha` is the previous remote HEAD (if any).
/// Thin packs delta against objects the receiver already has — much smaller for
/// incremental pushes. For fresh pushes (no old_sha), produces a full pack.
fn build_packfile(new_sha: &str, old_sha: &Option<String>) -> Vec<u8> {
    let mut child = std::process::Command::new("git")
        .args(["pack-objects", "--stdout", "--delta-base-offset", "--revs", "--thin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to run git pack-objects");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", new_sha).unwrap();
        if let Some(old) = old_sha {
            writeln!(stdin, "--not").unwrap();
            writeln!(stdin, "{}", old).unwrap();
        }
    }

    let output = child.wait_with_output().unwrap();
    if !output.status.success() {
        eprintln!(
            "git-remote-near: pack-objects failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output.stdout
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
