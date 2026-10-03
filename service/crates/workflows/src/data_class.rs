//! Data-class admission. The class always comes from the owner's repo config or research
//! scope, never from a default: a missing or unknown class is refused, and Employer data is
//! never processed by this runtime.
use pair_core::{
    error::{ErrorCode, PairError, Result},
    types::DataClass,
};

/// Maps the configured spelling to a data class this runtime may process.
pub fn require_data_class(raw: Option<&str>, what: &str) -> Result<DataClass> {
    let denied = |m: String| PairError::new(ErrorCode::PolicyDenied, m);
    let Some(raw) = raw else {
        return Err(denied(format!(
            "{what} must declare a data_class (public, personal or sensitive)"
        )));
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "public" => Ok(DataClass::Public),
        "personal" => Ok(DataClass::Personal),
        "sensitive" => Ok(DataClass::Sensitive),
        "employer" => Err(denied(format!(
            "{what}: employer data is not processed by PAIR"
        ))),
        other => Err(denied(format!("{what}: unknown data_class {other:?}"))),
    }
}
