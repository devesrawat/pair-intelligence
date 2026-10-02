use crate::config::ActionClass;
use pair_core::types::{ActionRequest, DataClass};
use serde::Serialize;
use sha2::{Digest, Sha256};

const PAYLOAD_SCHEMA: u32 = 1;

/// Fixed-field-order view of a request. The trace id is excluded so a retry of the same
/// operation hashes identically; everything that defines the operation is included.
#[derive(Serialize)]
struct CanonicalPayload<'a> {
    schema: u32,
    class: ActionClass,
    tool: &'a str,
    executable: Option<&'a str>,
    args: &'a [String],
    paths: &'a [String],
    destination: Option<&'a str>,
    data_class: DataClass,
    task: String,
}

/// Deterministic sha256 (hex) of the canonical request. Approvals bind to this value.
pub fn payload_hash(req: &ActionRequest, class: ActionClass) -> Result<String, serde_json::Error> {
    let canonical = CanonicalPayload {
        schema: PAYLOAD_SCHEMA,
        class,
        tool: &req.tool,
        executable: req.executable.as_deref(),
        args: &req.args,
        paths: &req.paths,
        destination: req.destination.as_deref(),
        data_class: req.data_class,
        task: req.task.to_string(),
    };
    let bytes = serde_json::to_vec(&canonical)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}
