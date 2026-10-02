use pair_core::error::{ErrorCode, PairError, Result};
use sha2::{Digest, Sha256};

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Hash of the exact operation payload an approval binds to. `serde_json::Value` objects are
/// key-sorted, so semantically equal payloads hash equally.
pub fn action_hash(payload: &serde_json::Value) -> Result<String> {
    let bytes = serde_json::to_vec(payload)
        .map_err(|e| PairError::new(ErrorCode::InvalidInput, format!("unserializable payload: {e}")))?;
    Ok(sha256_hex(&bytes))
}

pub(crate) fn is_valid_hash(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}
