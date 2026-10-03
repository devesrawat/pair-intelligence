//! Documented acceptance policy (spec section 7, lifecycle).
//!
//! Policy `memory-policy-v1`:
//! * Auto-accept ONLY when the candidate is an explicit (not inferred) `preference`, every
//!   evidence item has a verified span on an owner-trust, non-`sensitive` source, the text is
//!   not permission-like, and it contradicts nothing. Everything else waits in the inbox.
//! * Review is required for: inferred candidates, sensitive sources, decisions (architecture),
//!   contradictions, permission-like text, and any non-owner trust.
//! * A candidate can only become a `preference` (or carry permission-like text) when every
//!   span-verified evidence item is owner-trust and at least one is verified. A web
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
    /// The evidence span was found in supplied source text. An unverified citation proves
    /// nothing about the source it names, so its trust class is never relied on.
    pub span_verified: bool,
}

/// Trust of one evidence item as stored on a candidate, for the hard accept gate.
#[derive(Debug, Clone, Copy)]
pub struct EvidenceTrust {
    pub trust: TrustClass,
    pub span_verified: bool,
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
    if sources.iter().any(|s| !s.span_verified) {
        reasons.push("unverified_evidence");
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
///
/// Preferences and permission-like text need span-verified evidence, and EVERY verified cited
/// source must be owner-trust: one owner citation cannot launder a web page cited beside it.
pub fn check_accept(kind: &str, content: &str, evidence: &[EvidenceTrust]) -> Result<()> {
    if kind != "preference" && !looks_permission_like(content) {
        return Ok(());
    }
    let mut verified = evidence.iter().filter(|e| e.span_verified).peekable();
    let all_owner = verified.peek().is_some() && verified.all(|e| e.trust == TrustClass::Owner);
    if all_owner {
        Ok(())
    } else {
        Err(PairError::new(
            ErrorCode::PolicyDenied,
            "preferences and permissions require verified evidence from owner-trust sources only; untrusted, tool or unverified citations cannot change them",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: SourceFacts = SourceFacts {
        trust: TrustClass::Owner,
        data_class: DataClass::Personal,
        span_verified: true,
    };
    const fn ev(trust: TrustClass, span_verified: bool) -> EvidenceTrust {
        EvidenceTrust {
            trust,
            span_verified,
        }
    }

    #[test]
    fn test_assess_explicit_owner_preference_auto_accepts() {
        assert!(assess("preference", false, "Theme: dark", &[OWNER], false).auto_accept);
    }

    #[test]
    fn test_assess_unverified_owner_preference_needs_review() {
        let unverified = SourceFacts {
            span_verified: false,
            ..OWNER
        };
        let a = assess("preference", false, "Theme: dark", &[unverified], false);
        assert!(!a.auto_accept);
        assert!(a
            .review_reasons
            .contains(&"unverified_evidence".to_string()));
    }

    #[test]
    fn test_assess_inferred_preference_needs_review() {
        let a = assess("preference", true, "Theme: dark", &[OWNER], false);
        assert!(!a.auto_accept);
        assert!(a.review_reasons.contains(&"inferred".to_string()));
    }

    #[test]
    fn test_check_accept_untrusted_preference_denied() {
        let err = check_accept(
            "preference",
            "Theme: dark",
            &[ev(TrustClass::Untrusted, true)],
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::PolicyDenied);
    }

    #[test]
    fn test_check_accept_mixed_verified_sources_denied() {
        let err = check_accept(
            "preference",
            "Theme: dark",
            &[ev(TrustClass::Owner, true), ev(TrustClass::Untrusted, true)],
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::PolicyDenied);
    }

    #[test]
    fn test_check_accept_unverified_owner_preference_denied() {
        assert!(
            check_accept("preference", "Theme: dark", &[ev(TrustClass::Owner, false)]).is_err()
        );
    }

    #[test]
    fn test_check_accept_owner_verified_preference_allowed() {
        assert!(check_accept(
            "preference",
            "Theme: dark",
            &[
                ev(TrustClass::Owner, true),
                ev(TrustClass::Untrusted, false)
            ]
        )
        .is_ok());
    }

    #[test]
    fn test_check_accept_untrusted_fact_allowed() {
        assert!(check_accept(
            "fact",
            "The page says hello",
            &[ev(TrustClass::Untrusted, true)]
        )
        .is_ok());
    }
}
