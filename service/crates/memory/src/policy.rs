//! Documented acceptance policy (spec section 7, lifecycle).
//!
//! Policy `memory-policy-v1`:
//! * Auto-accept ONLY when the candidate is an explicit (not inferred) `preference`, every
//!   evidence source is owner-trust and not `sensitive`, the text is not permission-like,
//!   and it contradicts nothing. Everything else waits in the inbox.
//! * Review is required for: inferred candidates, sensitive sources, decisions (architecture),
//!   contradictions, permission-like text, and any non-owner trust.
//! * A candidate whose evidence has no owner-trust source can never become a `preference`,
//!   and permission-like text without owner evidence can never be accepted at all. A web
//!   page or tool output therefore cannot change preferences or standing permissions.
use crate::normalize::looks_permission_like;
use pair_core::{
    error::{ErrorCode, PairError, Result},
    types::{DataClass, TrustClass},
};

pub const AUTO_ACCEPT_ACTOR: &str = "policy:auto-accept-v1";

#[derive(Debug, Clone, Copy)]
pub struct SourceFacts {
    pub trust: TrustClass,
    pub data_class: DataClass,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub auto_accept: bool,
    pub review_reasons: Vec<String>,
}

pub fn assess(
    kind: &str,
    inferred: bool,
    content: &str,
    sources: &[SourceFacts],
    contradiction: bool,
) -> Assessment {
    let mut reasons: Vec<&str> = Vec::new();
    if sources.is_empty() {
        reasons.push("no_evidence");
    }
    if contradiction {
        reasons.push("contradiction");
    }
    if inferred {
        reasons.push("inferred");
    }
    if sources.iter().any(|s| s.data_class == DataClass::Sensitive) {
        reasons.push("sensitive_source");
    }
    if sources.iter().any(|s| s.trust == TrustClass::Untrusted) {
        reasons.push("untrusted_source");
    } else if sources.iter().any(|s| s.trust != TrustClass::Owner) {
        reasons.push("non_owner_source");
    }
    if looks_permission_like(content) {
        reasons.push("permission_like");
    }
    if kind == "decision" {
        reasons.push("decision");
    }
    if kind != "preference" {
        reasons.push("not_a_preference");
    }
    // Everything that is not auto-accepted is reviewed; `not_a_preference` alone is the
    // default path for ordinary facts and is recorded for transparency.
    let auto_accept = reasons.is_empty();
    let review_reasons = if auto_accept {
        Vec::new()
    } else {
        reasons.into_iter().map(str::to_string).collect()
    };
    Assessment {
        auto_accept,
        review_reasons,
    }
}

/// Hard gate applied on every acceptance, including human accepts.
pub fn check_accept(kind: &str, content: &str, trusts: &[TrustClass]) -> Result<()> {
    let has_owner = trusts.contains(&TrustClass::Owner);
    if has_owner {
        return Ok(());
    }
    if kind == "preference" || looks_permission_like(content) {
        return Err(PairError::new(
            ErrorCode::PolicyDenied,
            "preferences and permissions require owner-trust evidence; untrusted or tool sources cannot change them",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: SourceFacts = SourceFacts {
        trust: TrustClass::Owner,
        data_class: DataClass::Personal,
    };

    #[test]
    fn test_assess_explicit_owner_preference_auto_accepts() {
        assert!(assess("preference", false, "Theme: dark", &[OWNER], false).auto_accept);
    }

    #[test]
    fn test_assess_inferred_preference_needs_review() {
        let a = assess("preference", true, "Theme: dark", &[OWNER], false);
        assert!(!a.auto_accept);
        assert!(a.review_reasons.contains(&"inferred".to_string()));
    }

    #[test]
    fn test_check_accept_untrusted_preference_denied() {
        let err = check_accept("preference", "Theme: dark", &[TrustClass::Untrusted]).unwrap_err();
        assert_eq!(err.code, ErrorCode::PolicyDenied);
    }

    #[test]
    fn test_check_accept_untrusted_fact_allowed() {
        assert!(check_accept("fact", "The page says hello", &[TrustClass::Untrusted]).is_ok());
    }
}
