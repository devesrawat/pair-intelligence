//! Service-token auth: `Authorization: Bearer <PAIR_SERVICE_TOKEN>` plus `X-Actor`.

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use pair_core::error::{ErrorCode, PairError};
use subtle::ConstantTimeEq;

use crate::error::ApiError;
use crate::state::AppState;

pub const ACTOR_HEADER: &str = "x-actor";
const MAX_ACTOR_LEN: usize = 128;
const BEARER_PREFIX: &str = "bearer ";

/// Authenticated actor id from `X-Actor`.
#[derive(Clone, Debug)]
pub struct Actor(pub String);

fn unauthenticated(message: &str) -> Response {
    ApiError(PairError::new(ErrorCode::Unauthenticated, message)).into_response()
}

fn bearer_token(header: &str) -> Option<&str> {
    let prefix = header.get(..BEARER_PREFIX.len())?;
    if prefix.eq_ignore_ascii_case(BEARER_PREFIX) {
        header.get(BEARER_PREFIX.len()..)
    } else {
        None
    }
}

/// Constant-time equality (length mismatch short-circuits; length is not secret).
pub fn tokens_match(expected: &str, provided: &str) -> bool {
    expected.as_bytes().ct_eq(provided.as_bytes()).into()
}

fn valid_actor(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_ACTOR_LEN && !s.chars().any(char::is_control)
}

pub async fn require_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token);
    match token {
        Some(t) if tokens_match(state.token(), t.trim()) => {}
        _ => return unauthenticated("missing or invalid service token"),
    }
    let actor = req
        .headers()
        .get(ACTOR_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|a| valid_actor(a))
        .map(str::to_owned);
    match actor {
        Some(a) => {
            req.extensions_mut().insert(Actor(a));
            next.run(req).await
        }
        None => unauthenticated("missing or invalid X-Actor header"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokens_match_equal_and_unequal() {
        assert!(tokens_match("secret-token", "secret-token"));
        assert!(!tokens_match("secret-token", "secret-tokeN"));
        assert!(!tokens_match("secret-token", "secret"));
        assert!(!tokens_match("secret-token", ""));
    }

    #[test]
    fn test_bearer_token_parses_scheme_case_insensitively() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bear"), None);
    }
}
