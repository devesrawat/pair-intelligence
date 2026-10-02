use sha2::{Digest, Sha256};
use std::fmt::Write;

/// Lowercase hex SHA-256 of `text`.
pub fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}
