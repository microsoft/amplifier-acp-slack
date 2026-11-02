use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String, pub Option<Value>);
impl ApiError {
    pub fn bad(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into(), None)
    }
    pub fn unauthorized() -> Self {
        Self(
            StatusCode::UNAUTHORIZED,
            "Sign in or supply a valid access token".into(),
            None,
        )
    }
    pub fn forbidden() -> Self {
        Self(
            StatusCode::FORBIDDEN,
            "This operation is not allowed by this credential".into(),
            None,
        )
    }
    pub fn missing() -> Self {
        Self(StatusCode::NOT_FOUND, "Resource not found".into(), None)
    }
    pub fn conflict(value: Value) -> Self {
        Self(
            StatusCode::CONFLICT,
            "The resource changed; read it again before saving".into(),
            Some(value),
        )
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1,"detail":self.2}))).into_response()
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(_: anyhow::Error) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The operation could not be completed; inspect the service log".into(),
            None,
        )
    }
}
impl From<rusqlite::Error> for ApiError {
    fn from(_: rusqlite::Error) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The local state store is unavailable".into(),
            None,
        )
    }
}
impl From<std::io::Error> for ApiError {
    fn from(_: std::io::Error) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The adapter files could not be accessed".into(),
            None,
        )
    }
}
pub type ApiResult<T> = Result<T, ApiError>;
