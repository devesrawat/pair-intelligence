//! JSON body extractor whose rejections use the standard error envelope.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use pair_core::error::{ErrorCode, PairError};
use serde::de::DeserializeOwned;

use crate::error::ApiError;

pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Self(v)),
            Err(rejection) => Err(ApiError(PairError::new(
                ErrorCode::InvalidInput,
                rejection_message(&rejection),
            ))),
        }
    }
}

fn rejection_message(r: &JsonRejection) -> String {
    match r {
        JsonRejection::MissingJsonContentType(_) => "content-type must be application/json".into(),
        JsonRejection::JsonSyntaxError(_) => "request body is not valid JSON".into(),
        JsonRejection::JsonDataError(e) => format!("invalid request body: {e}"),
        _ => "request body could not be read".into(),
    }
}
