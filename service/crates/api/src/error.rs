//! HTTP mapping for `PairError` using the standard response envelope.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use pair_core::error::{ErrorCode, PairError};
use serde_json::json;

pub struct ApiError(pub PairError);

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
        let body = json!({
            "success": false,
            "data": null,
            "error": { "code": self.0.code, "message": self.0.message },
        });
        (status, Json(body)).into_response()
    }
}
