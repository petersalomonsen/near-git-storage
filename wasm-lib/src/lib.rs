use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use wasm_bindgen::prelude::*;

/// Parse a packfile and return objects + deltas as JSON.
#[wasm_bindgen]
pub fn parse_packfile(data: &[u8]) -> Result<String, String> {
    let result = git_core::packfile::parse(data)?;

    let objects: Vec<serde_json::Value> = result
        .objects
        .iter()
        .map(|obj| {
            serde_json::json!({
                "obj_type": obj.obj_type,
                "data": base64::engine::general_purpose::STANDARD.encode(&obj.data),
            })
        })
        .collect();

    let deltas: Vec<serde_json::Value> = result
        .deltas
        .iter()
        .map(|d| {
            serde_json::json!({
                "base_sha": d.base_sha,
                "delta_data": base64::engine::general_purpose::STANDARD.encode(&d.delta_data),
            })
        })
        .collect();

    serde_json::to_string(&serde_json::json!({
        "objects": objects,
        "deltas": deltas,
    }))
    .map_err(|e| e.to_string())
}

/// Build a packfile from objects (JSON array of {obj_type, data(base64)}).
#[wasm_bindgen]
pub fn build_packfile(objects_json: &str) -> Result<Vec<u8>, String> {
    let objects: Vec<serde_json::Value> =
        serde_json::from_str(objects_json).map_err(|e| e.to_string())?;

    let pack_objects: Vec<git_core::packfile::PackObject> = objects
        .iter()
        .map(|obj| {
            let obj_type = obj["obj_type"].as_str().unwrap_or("blob").to_string();
            let data_b64 = obj["data"].as_str().unwrap_or("");
            let data = base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .unwrap_or_default();
            git_core::packfile::PackObject { obj_type, data }
        })
        .collect();

    Ok(git_core::packfile::build(&pack_objects))
}

/// Apply a binary delta to a source object.
#[wasm_bindgen]
pub fn apply_delta(source: &[u8], delta: &[u8]) -> Result<Vec<u8>, String> {
    git_core::packfile::apply_delta(source, delta)
}

/// Compute the git SHA-1 hash for an object.
#[wasm_bindgen]
pub fn git_sha1(obj_type: &str, data: &[u8]) -> String {
    let obj = git_core::packfile::PackObject {
        obj_type: obj_type.to_string(),
        data: data.to_vec(),
    };
    obj.sha1()
}

// --- NEAR transaction signing ---

/// Create a signed NEAR function call transaction, returned as base64.
///
/// - `signer_id`: e.g. "alice.near"
/// - `public_key_b58`: base58-encoded ed25519 public key (without "ed25519:" prefix)
/// - `private_key_b58`: base58-encoded ed25519 private key (without "ed25519:" prefix)
/// - `receiver_id`: contract account, e.g. "repo.near"
/// - `method_name`: e.g. "push_objects"
/// - `args_json`: JSON string of method arguments
/// - `nonce`: access key nonce + 1
/// - `block_hash_b58`: recent block hash in base58
/// - `gas`: gas to attach (e.g. 300000000000000 = 300 TGas)
/// - `deposit`: attached deposit in yoctoNEAR (as string, e.g. "0")
#[wasm_bindgen]
pub fn create_signed_transaction(
    signer_id: &str,
    public_key_b58: &str,
    private_key_b58: &str,
    receiver_id: &str,
    method_name: &str,
    args_json: &str,
    nonce: u64,
    block_hash_b58: &str,
    gas: u64,
    deposit: &str,
) -> Result<String, String> {
    let private_key_bytes = bs58::decode(private_key_b58)
        .into_vec()
        .map_err(|e| format!("bad private key: {}", e))?;

    // ed25519 secret key is 64 bytes (32 secret + 32 public) or 32 bytes (just secret)
    let signing_key = if private_key_bytes.len() == 64 {
        SigningKey::from_keypair_bytes(
            private_key_bytes
                .as_slice()
                .try_into()
                .map_err(|_| "invalid key length")?,
        )
        .map_err(|e| format!("bad keypair: {}", e))?
    } else if private_key_bytes.len() == 32 {
        SigningKey::from_bytes(
            private_key_bytes
                .as_slice()
                .try_into()
                .map_err(|_| "invalid key length")?,
        )
    } else {
        return Err(format!(
            "unexpected key length: {} (expected 32 or 64)",
            private_key_bytes.len()
        ));
    };

    let public_key_bytes = bs58::decode(public_key_b58)
        .into_vec()
        .map_err(|e| format!("bad public key: {}", e))?;

    let block_hash_bytes = bs58::decode(block_hash_b58)
        .into_vec()
        .map_err(|e| format!("bad block hash: {}", e))?;
    if block_hash_bytes.len() != 32 {
        return Err(format!(
            "block hash must be 32 bytes, got {}",
            block_hash_bytes.len()
        ));
    }

    let deposit: u128 = deposit.parse().map_err(|e| format!("bad deposit: {}", e))?;

    // Borsh-serialize the Transaction
    let mut tx_bytes = Vec::new();

    // signer_id: String
    borsh_write_string(&mut tx_bytes, signer_id);
    // public_key: enum(0=ed25519) + 32 bytes
    tx_bytes.push(0); // ED25519
    tx_bytes.extend_from_slice(&public_key_bytes);
    // nonce: u64 LE
    tx_bytes.extend_from_slice(&nonce.to_le_bytes());
    // receiver_id: String
    borsh_write_string(&mut tx_bytes, receiver_id);
    // block_hash: 32 bytes
    tx_bytes.extend_from_slice(&block_hash_bytes);
    // actions: Vec<Action> with length prefix
    tx_bytes.extend_from_slice(&1u32.to_le_bytes()); // 1 action
    // Action::FunctionCall = enum tag 2
    tx_bytes.push(2);
    // method_name: String
    borsh_write_string(&mut tx_bytes, method_name);
    // args: Vec<u8>
    let args = args_json.as_bytes();
    tx_bytes.extend_from_slice(&(args.len() as u32).to_le_bytes());
    tx_bytes.extend_from_slice(args);
    // gas: u64 LE
    tx_bytes.extend_from_slice(&gas.to_le_bytes());
    // deposit: u128 LE
    tx_bytes.extend_from_slice(&deposit.to_le_bytes());

    // Hash and sign
    let tx_hash = Sha256::digest(&tx_bytes);
    let signature = signing_key.sign(tx_hash.as_slice());

    // Borsh-serialize SignedTransaction
    let mut signed_bytes = Vec::new();
    // transaction bytes
    signed_bytes.extend_from_slice(&tx_bytes);
    // signature: enum(0=ed25519) + 64 bytes
    signed_bytes.push(0); // ED25519
    signed_bytes.extend_from_slice(&signature.to_bytes());

    Ok(base64::engine::general_purpose::STANDARD.encode(&signed_bytes))
}

fn borsh_write_string(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
    buf.extend_from_slice(s.as_bytes());
}
