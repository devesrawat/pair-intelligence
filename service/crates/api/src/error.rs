//! HTTP mapping for `PairError` using the standard response envelope.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use pair_core::error::{ErrorCode, PairError};
use serde_json::json;

pub struct ApiError(pub PairError);

/// What an `Internal` error says to the caller. The real text (often sqlx or filesystem detail)
/// stays server-side: it travels in [`InternalDetail`] to the trace layer, which logs it with the
/// trace id.
pub const INTERNAL_MESSAGE: &str = "internal error";

/// Server-side detail of an `Internal` error, attached to the response extensions.
#[derive(Debug, Clone)]
pub struct InternalDetail(pub String);

/// Boundary failures: a body that parsed but is semantically invalid is 422, everything else maps
/// as [`ApiError`] does.
pub enum BoundaryError {
    Unprocessable(PairError),
    Api(ApiError),
}

impl BoundaryError {
    /// `InvalidInput` from request validation is 422; any other code (a refused data class) keeps
    /// its own status.
    pub fn from_validation(error: PairError) -> Self {
        if error.code == ErrorCode::InvalidInput {
            Self::Unprocessable(error)
        } else {
            Self::Api(ApiError(error))
        }
    }
}

impl From<ApiError> for BoundaryError {
    fn from(value: ApiError) -> Self {
        Self::Api(value)
    }
}

impl From<PairError> for BoundaryError {
    fn from(value: PairError) -> Self {
        Self::Api(ApiError(value))
    }
}

fn envelope(status: StatusCode, error: &PairError) -> Response {
    let body = json!({
        "success": false,
        "data": null,
        "error": { "code": error.code, "message": error.message },
    });
    (status, Json(body)).into_response()
}

impl IntoResponse for BoundaryError {
    fn into_response(self) -> Response {
        match self {
            Self::Unprocessable(e) => envelope(StatusCode::UNPROCESSABLE_ENTITY, &e),
            Self::Api(e) => e.into_response(),
        }
    }
}

impl From<PairError> for ApiError {
    fn from(value: PairError) -> Self {
        Self(value)
    }
}

fn status_for(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::Unauthenticated => StatusCode::UNAUTHORIZED,
        ErrorCode::PolicyDenied | ErrorCode::ProviderDisallowed => StatusCode::FORBIDDEN,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::InvalidInput => StatusCode::BAD_REQUEST,
        ErrorCode::Conflict
        | ErrorCode::ApprovalPayloadChanged
        | ErrorCode::ReservationUnresolved => StatusCode::CONFLICT,
        ErrorCode::BudgetExceeded => StatusCode::PAYMENT_REQUIRED,
        ErrorCode::BudgetUnknownPrice
        | ErrorCode::ContextOverflow
        | ErrorCode::CapabilityMismatch => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::ApprovalRequired => StatusCode::FORBIDDEN,
        ErrorCode::ApprovalExpired => StatusCode::GONE,
        ErrorCode::LimitExceeded => StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::ClassifierInvalid => StatusCode::BAD_GATEWAY,
        ErrorCode::ProviderUnavailable | ErrorCode::ProviderTimeout => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = status_for(self.0.code);
        if self.0.code != ErrorCode::Internal {
            return envelope(status, &self.0);
        }
        let public = PairError::new(ErrorCode::Internal, INTERNAL_MESSAGE);
        let mut response = envelope(status, &public);
        response
            .extensions_mut()
            .insert(InternalDetail(self.0.message));
        response
    }
}
